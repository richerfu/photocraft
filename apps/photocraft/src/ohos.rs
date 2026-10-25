//! Ability lifecycle host. All editor UI and document operations remain in Rust.
use openharmony_ability::OpenHarmonyApp;
use photocraft_engine::Session;
use photocraft_ui_egui::{FileDialogAnswer, FileDialogRequest, PhotocraftApp};
use std::{path::PathBuf, sync::Arc};
use winit::platform::ohos::plugins::{clipboard::ClipboardClient, permission::PermissionExt as _};
use winit::platform::ohos::{
    plugins::{
        files::{FileDialogFilter, FileDialogOptions, FilesExt as _},
        url::UrlExt as _,
    },
    task,
};

pub fn start(ability: OpenHarmonyApp) -> Result<(), String> {
    let logger = crate::logging::install();
    crate::crash_guard::install_hook();
    let directory = ability.pref_path().or_else(|| ability.base_path()).map(PathBuf::from).ok_or("Ability did not provide an application data directory")?;
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    crate::app_dirs::set_ohos_dir(directory.clone());
    if let Some(logger) = logger {
        match logger.attach_dir(&directory.join("logs")) {
            Ok(path) => log::info!("PhotoCraft {}, log file {}", photocraft_engine::build_info::long_version(), path.display()),
            Err(e) => log::warn!("no log file: {e}"),
        }
    }
    let monitor = crate::monitor_profile::detect_async();
    // Fontique scans all installed families. On an emulator this can take several
    // seconds; finish it before allowing text tools/imports to touch the shared DB.
    let grants = Arc::new(crate::ohos_files::Grants::new(directory.clone()));
    let startup_grants = grants.clone();
    let (fonts_tx, fonts_ready) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("photocraft-fonts".into())
        .spawn(move || {
            startup_grants.restore();
            let result = std::panic::catch_unwind(photocraft_ui_egui::type_tool::families)
                .map(|_| ())
                .map_err(|_| "Unable to load installed fonts. Restart PhotoCraft to retry.".to_owned());
            let _ = fonts_tx.send(result);
        })
        .map_err(|error| error.to_string())?;
    let mut options = eframe::NativeOptions {
        openharmony_app: Some(ability.clone()),
        viewport: egui::ViewportBuilder::default()
            .with_title("PhotoCraft")
            .with_app_id("ai.storyteller.photocraft")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([760.0, 480.0])
            .with_drag_and_drop(true),
        persistence_path: Some(directory.join("ui.ron")),
        ..Default::default()
    };
    crate::ui_state::sanitize(options.persistence_path.as_deref());
    let t_sentinel = std::time::Instant::now();
    let os = crate::gpu_startup::Os::current();
    let (pref, mode) = crate::gpu_startup::read_rendering_prefs(crate::services::prefs_file().as_deref());
    let (previous, sentinel) = crate::gpu_startup::Sentinel::begin(&directory);
    let plan = crate::gpu_startup::plan_with_mode(pref, mode, previous.crashed(), None, false, os);
    if let Some(m) = previous.crashed() {
        log::warn!("the previous start didn't finish (GPU backend {}, adapter {:?}); {}", m.backend, m.adapter, plan.reason.as_deref().unwrap_or(""));
    }
    let sentinel: crate::gpu_startup::SharedSentinel = Arc::new(std::sync::Mutex::new(sentinel));
    if let Some(s) = sentinel.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut() {
        let marker = crate::gpu_startup::Marker {
            backend: plan.backend.name().to_string(),
            version: photocraft_engine::build_info::long_version().to_string(),
            ..Default::default()
        };
        if let Err(e) = s.write(marker) {
            log::warn!("GPU startup marker: {e}");
        }
    }
    let gpu_note: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    crate::gpu_startup::configure(&mut options.wgpu_options.wgpu_setup, &plan, os, sentinel.clone(), gpu_note.clone());
    // API 26's emulator exposes GLES; the editor can still use its wgpu canvas on this backend.
    if plan.env.is_none() {
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
            setup.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
    }
    let sentinel_ms = t_sentinel.elapsed().as_secs_f64() * 1000.0;
    log::info!("GPU startup: {:?} ({sentinel_ms:.2} ms)", plan);
    let keep_marker = crate::gpu_startup::keep_marker_after_error(&plan, os);
    let started_sentinel = sentinel.clone();
    #[cfg(feature = "ohos-control")]
    let control = {
        let root = directory.join("validation");
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let workspace = photocraft_automation::AuthorizedWorkspace::new(Some(&root), Some(&root)).map_err(|error| error.to_string())?;
        let token = include_str!(env!("PHOTOCRAFT_OHOS_CONTROL_TOKEN_FILE")).trim().to_owned();
        Some((workspace, token))
    };
    #[cfg(not(feature = "ohos-control"))]
    let control: Option<(photocraft_automation::AuthorizedWorkspace, String)> = None;
    // The launch Want can arrive before eframe creates the first rendering surface.
    let events = winit::platform::ohos::events::subscribe();
    let result = eframe::run_native(
        "PhotoCraft",
        options,
        Box::new(move |cc| {
            let egui_ctx = cc.egui_ctx.clone();
            let render_state = cc.wgpu_render_state.clone();
            let cursor_pos = crate::cursor::service(cc);
            let screen_pick = crate::screen_color::service(cc, false);
            let create = move || {
                let mut services = crate::services::native(control.as_ref().map(|(workspace, _)| workspace.clone()));
                services.preset_store = crate::services::presets_dir().map(photocraft_engine::preset_store::open_dir_async);
                services.cursor_pos = cursor_pos;
                services.screen_pick = screen_pick;
                let app = ability.clone();
                let save_grants = grants.clone();
                services.save_needs_authorization = Some(Box::new(move |path| !save_grants.can_save(std::path::Path::new(path))));
                services.file_dialog = Some(Box::new(move |request, _parent, reply| {
                    let app = app.clone();
                    let grants = grants.clone();
                    task::spawn(async move {
                        let result: Result<Option<FileDialogAnswer>, String> = async {
                            if let FileDialogRequest::AuthorizeSave { path } = &request {
                                return Ok(grants.authorize_save(&app, std::path::Path::new(path)).await?.then(|| FileDialogAnswer::SaveTo(path.clone())));
                            }
                            let save = matches!(request, FileDialogRequest::Save { .. });
                            let options = match request {
                                FileDialogRequest::Open { multiple } => FileDialogOptions::new("open-file")
                                    .allow_many(multiple)
                                    .filters(vec![FileDialogFilter::new().name("All Formats").pattern(crate::services::OPEN_EXTS.join(";"))]),
                                FileDialogRequest::Save { suggested } => FileDialogOptions::new("save-file")
                                    .filters(
                                        crate::services::save_filters(&suggested)
                                            .into_iter()
                                            .map(|(name, extensions)| FileDialogFilter::new().name(name).pattern(extensions.join(";")))
                                            .collect(),
                                    )
                                    .suggested_name(
                                        std::path::Path::new(&suggested)
                                            .file_name()
                                            .map_or_else(|| "Untitled.psd".into(), |name| name.to_string_lossy().into_owned()),
                                    ),
                                FileDialogRequest::AuthorizeSave { .. } => return Err("Unexpected save authorization request".into()),
                            };
                            let response = app.show_file_dialog(options).await.map_err(|error| error.to_string())?;
                            if save {
                                let Some(uri) = response.files.first() else { return Ok(None) };
                                let path = winit::platform::ohos::drag_drop::file_path(uri).ok_or("Cannot resolve selected save URI")?;
                                return Ok(grants.authorize_save(&app, &path).await?.then(|| FileDialogAnswer::SaveTo(path.to_string_lossy().into_owned())));
                            }
                            let paths = grants.persist(&response.files, false)?;
                            Ok((!paths.is_empty()).then(|| FileDialogAnswer::Paths(paths.into_iter().map(|p| p.to_string_lossy().into_owned()).collect())))
                        }
                        .await;
                        match result {
                            Ok(answer) => reply.send(answer),
                            Err(error) => {
                                log::error!("PhotoCraft file dialog failed: {error}");
                                reply.send_error(error);
                            }
                        }
                    });
                }));
                let app = ability.clone();
                services.open_url = Some(Box::new(move |url| {
                    let future = app.open_url(url);
                    task::spawn(async move {
                        if let Err(error) = future.await {
                            log::warn!("Open URL failed: {error}");
                        }
                    });
                    Ok(())
                }));
                let commands = Arc::new(task::CommandQueue::default());
                let clipboard_app = ability.clone();
                let writes = commands.clone();
                services.clipboard_set_image = Some(Box::new(move |width, height, bytes| {
                    let app = clipboard_app.clone();
                    let bytes = bytes.to_vec();
                    writes.push(async move {
                        let result = async { ClipboardClient::new(&app)?.write_image(&bytes, width, height).await }.await;
                        if let Err(error) = result {
                            log::warn!("Image clipboard write failed: {error}");
                        }
                    });
                    Ok(())
                }));
                let clipboard_app = ability.clone();
                let clipboard_ctx = egui_ctx.clone();
                services.clipboard_read_async = Some(Box::new(move || {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let app = clipboard_app.clone();
                    let ctx = clipboard_ctx.clone();
                    commands.push(async move {
                        let result: Result<_, String> = async {
                            app.request_permission("ohos.permission.READ_PASTEBOARD").await.map_err(|error| error.to_string())?;
                            let contents =
                                ClipboardClient::new(&app).map_err(|error| error.to_string())?.read_content().await.map_err(|error| error.to_string())?;
                            let png = match contents.png {
                                Some(png) => Some(png),
                                None => contents
                                    .uris
                                    .iter()
                                    .filter_map(|uri| winit::platform::ohos::drag_drop::file_path(uri))
                                    .find_map(|path| std::fs::read(path).ok()),
                            };
                            png.map(|bytes| {
                                let image = photocraft_codecs::decode(&bytes).map_err(|error| error.to_string())?;
                                let rgba = image.convert(photocraft_codecs::ChannelLayout::Rgba, photocraft_codecs::SampleType::U8);
                                Ok((rgba.width(), rgba.height(), rgba.data().to_vec()))
                            })
                            .transpose()
                        }
                        .await;
                        let _ = tx.send(result);
                        ctx.request_repaint();
                    });
                    rx
                }));
                services.os_events = Some(Box::new(move || {
                    events
                        .try_iter()
                        .filter_map(|event| match event {
                            winit::platform::ohos::events::PlatformEvent::Want { uri, .. } if !uri.is_empty() => {
                                winit::platform::ohos::drag_drop::file_path(&uri)
                                    .map(|path| photocraft_ui_egui::OsEvent::Open(vec![path.to_string_lossy().into_owned()]))
                            }
                            _ => None,
                        })
                        .collect()
                }));
                let mut app = PhotocraftApp::new(Session::new(), services);
                app.background_jobs = true;
                if let Some(rx) = monitor {
                    app.services.read_displays = Some(Arc::new(crate::monitor_profile::detect_async));
                    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
                        Ok(r) => photocraft_ui_egui::monitor_status::apply(&mut app, r),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => photocraft_ui_egui::monitor_status::pending(&mut app, rx),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            photocraft_ui_egui::monitor_status::apply(&mut app, Err("the display profile reader stopped without an answer".into()));
                        }
                    }
                }
                app.perf.gpu_info.preference = pref.name().to_string();
                app.perf.gpu_info.selected = if plan.env.is_some() { "env".into() } else { plan.backend.name().to_string() };
                app.perf.gpu_info.fallback = match (plan.reason.clone(), gpu_note.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()) {
                    (Some(reason), Some(note)) => Some(format!("{reason}. {note}")),
                    (reason, note) => reason.or(note),
                };
                app.perf.gpu_info.canvas = "cpu".into();
                if let Some(rs) = render_state.clone() {
                    app.perf.gpu_info.set_adapter(&rs.adapter.get_info());
                    if plan.backend != photocraft_engine::prefs::GpuBackend::Cpu
                        && app.session.prefs().performance.effective_rendering_mode() != photocraft_engine::prefs::RenderingMode::Cpu
                    {
                        app.perf.gpu_info.selected = "gl".into();
                        app.set_wgpu(rs);
                    } else {
                        let _ = photocraft_ui_egui::gpu_canvas::DeviceHealth::watch(&rs.device);
                    }
                }
                if app.perf.gpu_info.canvas == "cpu"
                    && mode != photocraft_engine::prefs::RenderingMode::Cpu
                    && let Some(reason) = app.perf.gpu_info.fallback.clone()
                {
                    photocraft_ui_egui::gpu_status::queue_fallback_notice(&mut app, &reason);
                }
                app.perf.span("gpuSentinel", sentinel_ms);
                let remember_cpu = plan.remember && plan.backend == photocraft_engine::prefs::GpuBackend::Cpu;
                let remember = plan.remember.then_some(plan.backend);
                app.on_started(move |app| {
                    if let Some(s) = started_sentinel.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
                        s.finish();
                    }
                    if remember_cpu
                        && let Err(error) = app.run(
                            "prefs.set",
                            serde_json::json!({"values": {
                                "performance.renderingMode": "cpu", "performance.useGpu": false
                            }}),
                        )
                    {
                        log::warn!("couldn't remember CPU recovery: {error}");
                    }
                    if let Some(b) = remember
                        && app.session.prefs().performance.gpu_backend != b
                        && let Err(e) = app.run("prefs.set", serde_json::json!({"path": "performance.gpuBackend", "value": b.name()}))
                    {
                        log::warn!("couldn't remember GPU backend {}: {e}", b.name());
                    }
                });
                if let Some((_, token)) = control {
                    app = app.with_control(crate::control_server::start(7878, token, egui_ctx.clone()));
                }
                log::info!("PhotoCraft native editor ready");
                app
            };
            Ok(Box::new(StartupApp { fonts_ready, create: Some(Box::new(create)), app: None, error: None }))
        }),
    );
    let renderer_error = crate::gpu_startup::keep_marker_after_run(&result);
    if !(renderer_error && keep_marker)
        && let Some(mut s) = sentinel.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
    {
        if result.is_err()
            && !renderer_error
            && let Some(marker) = previous.crashed()
        {
            if let Err(error) = s.write(marker.clone()) {
                log::warn!("couldn't restore previous GPU startup marker: {error}");
            }
        } else {
            s.finish();
        }
    }
    result.map_err(|error| error.to_string())
}

/// Keep the Ability's UI thread responsive while the native font catalogue loads.
struct StartupApp {
    fonts_ready: std::sync::mpsc::Receiver<Result<(), String>>,
    create: Option<Box<dyn FnOnce() -> PhotocraftApp>>,
    app: Option<PhotocraftApp>,
    error: Option<String>,
}

impl eframe::App for StartupApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.app.is_none() && self.error.is_none() {
            match self.fonts_ready.try_recv() {
                Ok(Ok(())) => self.app = self.create.take().map(|create| create()),
                Ok(Err(error)) => self.error = Some(error),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.error = Some("Font loading stopped. Restart PhotoCraft to retry.".into()),
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        if let Some(app) = &mut self.app {
            app.logic(ctx, frame);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if let Some(app) = &mut self.app {
            app.ui(ui, frame);
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.4);
                    ui.heading("PhotoCraft");
                    if let Some(error) = &self.error {
                        ui.label(error);
                    } else {
                        ui.spinner();
                        ui.label("Loading installed fonts…");
                    }
                });
            });
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        if let Some(app) = &mut self.app {
            app.raw_input_hook(ctx, input);
        }
    }
}
