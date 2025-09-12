use crate::CustomEvent;
use crate::cef::WindowSize;
use crate::consts::{APP_NAME, CEF_MESSAGE_LOOP_MAX_ITERATIONS};
use crate::persist::PersistentData;
use crate::render::GraphicsState;
use graphite_desktop_wrapper::messages::{DesktopFrontendMessage, DesktopWrapperMessage, Platform};
use graphite_desktop_wrapper::{DesktopWrapper, NodeGraphExecutionResult, WgpuContext, serialize_frontend_messages};

use rfd::AsyncFileDialog;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::event_loop::ControlFlow;
use winit::event_loop::EventLoopProxy;
use winit::window::Window;
use winit::window::WindowId;

use crate::cef;

pub(crate) struct WinitApp {
	cef_context: Box<dyn cef::CefContext>,
	window: Option<Arc<Window>>,
	cef_schedule: Option<Instant>,
	window_size_sender: Sender<WindowSize>,
	graphics_state: Option<GraphicsState>,
	wgpu_context: WgpuContext,
	event_loop_proxy: EventLoopProxy<CustomEvent>,
	desktop_wrapper: DesktopWrapper,
	last_ui_update: Instant,
	avg_frame_time: f32,
	start_render_sender: SyncSender<()>,
	web_communication_initialized: bool,
	web_communication_startup_buffer: Vec<Vec<u8>>,
	persistent_data: PersistentData,
	chrome: Option<hybrid_chrome::HybridChromeHandle>,
}

impl WinitApp {
	pub(crate) fn new(cef_context: Box<dyn cef::CefContext>, window_size_sender: Sender<WindowSize>, wgpu_context: WgpuContext, event_loop_proxy: EventLoopProxy<CustomEvent>) -> Self {
		let rendering_loop_proxy = event_loop_proxy.clone();
		let (start_render_sender, start_render_receiver) = std::sync::mpsc::sync_channel(1);
		std::thread::spawn(move || {
			loop {
				let result = futures::executor::block_on(DesktopWrapper::execute_node_graph());
				let _ = rendering_loop_proxy.send_event(CustomEvent::NodeGraphExecutionResult(result));
				let _ = start_render_receiver.recv();
			}
		});

		let mut persistent_data = PersistentData::default();
		persistent_data.load_from_disk();

		Self {
			cef_context,
			window: None,
			cef_schedule: Some(Instant::now()),
			graphics_state: None,
			window_size_sender,
			wgpu_context,
			event_loop_proxy,
			desktop_wrapper: DesktopWrapper::new(),
			last_ui_update: Instant::now(),
			avg_frame_time: 0.,
			start_render_sender,
			web_communication_initialized: false,
			web_communication_startup_buffer: Vec::new(),
			persistent_data,
			chrome: None,
		}
	}

	fn handle_desktop_frontend_message(&mut self, message: DesktopFrontendMessage) {
		match message {
			DesktopFrontendMessage::ToWeb(messages) => {
				let Some(bytes) = serialize_frontend_messages(messages) else {
					tracing::error!("Failed to serialize frontend messages");
					return;
				};
				self.send_or_queue_web_message(bytes);
			}
			DesktopFrontendMessage::OpenFileDialog { title, filters, context } => {
				let event_loop_proxy = self.event_loop_proxy.clone();
				let _ = thread::spawn(move || {
					let mut dialog = AsyncFileDialog::new().set_title(title);
					for filter in filters {
						dialog = dialog.add_filter(filter.name, &filter.extensions);
					}

					let show_dialog = async move { dialog.pick_file().await.map(|f| f.path().to_path_buf()) };

					if let Some(path) = futures::executor::block_on(show_dialog)
						&& let Ok(content) = std::fs::read(&path)
					{
						let message = DesktopWrapperMessage::OpenFileDialogResult { path, content, context };
						let _ = event_loop_proxy.send_event(CustomEvent::DesktopWrapperMessage(message));
					}
				});
			}
			DesktopFrontendMessage::SaveFileDialog {
				title,
				default_filename,
				default_folder,
				filters,
				context,
			} => {
				let event_loop_proxy = self.event_loop_proxy.clone();
				let _ = thread::spawn(move || {
					let mut dialog = AsyncFileDialog::new().set_title(title).set_file_name(default_filename);
					if let Some(folder) = default_folder {
						dialog = dialog.set_directory(folder);
					}
					for filter in filters {
						dialog = dialog.add_filter(filter.name, &filter.extensions);
					}

					let show_dialog = async move { dialog.save_file().await.map(|f| f.path().to_path_buf()) };

					if let Some(path) = futures::executor::block_on(show_dialog) {
						let message = DesktopWrapperMessage::SaveFileDialogResult { path, context };
						let _ = event_loop_proxy.send_event(CustomEvent::DesktopWrapperMessage(message));
					}
				});
			}
			DesktopFrontendMessage::WriteFile { path, content } => {
				if let Err(e) = std::fs::write(&path, content) {
					tracing::error!("Failed to write file {}: {}", path.display(), e);
				}
			}
			DesktopFrontendMessage::OpenUrl(url) => {
				let _ = thread::spawn(move || {
					if let Err(e) = open::that(&url) {
						tracing::error!("Failed to open URL: {}: {}", url, e);
					}
				});
			}
			DesktopFrontendMessage::UpdateViewportBounds { x, y, width, height } => {
				if let Some(graphics_state) = &mut self.graphics_state
					&& let Some(window) = &self.window
				{
					let window_size = window.inner_size();

					let viewport_offset_x = x / window_size.width as f32;
					let viewport_offset_y = y / window_size.height as f32;
					graphics_state.set_viewport_offset([viewport_offset_x, viewport_offset_y]);

					let viewport_scale_x = if width != 0.0 { window_size.width as f32 / width } else { 1.0 };
					let viewport_scale_y = if height != 0.0 { window_size.height as f32 / height } else { 1.0 };
					graphics_state.set_viewport_scale([viewport_scale_x, viewport_scale_y]);
				}
			}
			DesktopFrontendMessage::UpdateOverlays(scene) => {
				if let Some(graphics_state) = &mut self.graphics_state {
					graphics_state.set_overlays_scene(scene);
				}
			}
			DesktopFrontendMessage::UpdateWindowState { maximized, minimized } => {
				if let Some(window) = &self.window {
					window.set_maximized(maximized);
					window.set_minimized(minimized);
				}
			}
			DesktopFrontendMessage::DragWindow => {
				if let Some(window) = &self.window {
					let _ = window.drag_window();
				}
			}
			DesktopFrontendMessage::CloseWindow => {
				let _ = self.event_loop_proxy.send_event(CustomEvent::CloseWindow);
			}
			DesktopFrontendMessage::PersistenceWriteDocument { id, document } => {
				self.persistent_data.write_document(id, document);
			}
			DesktopFrontendMessage::PersistenceDeleteDocument { id } => {
				self.persistent_data.delete_document(&id);
			}
			DesktopFrontendMessage::PersistenceUpdateCurrentDocument { id } => {
				self.persistent_data.set_current_document(id);
			}
			DesktopFrontendMessage::PersistenceUpdateDocumentsList { ids } => {
				self.persistent_data.set_document_order(ids);
			}
			DesktopFrontendMessage::PersistenceLoadCurrentDocument => {
				if let Some((id, document)) = self.persistent_data.current_document() {
					let message = DesktopWrapperMessage::LoadDocument {
						id,
						document,
						to_front: false,
						select_after_open: true,
					};
					self.dispatch_desktop_wrapper_message(message);
				}
			}
			DesktopFrontendMessage::PersistenceLoadRemainingDocuments => {
				for (id, document) in self.persistent_data.documents_before_current().into_iter().rev() {
					let message = DesktopWrapperMessage::LoadDocument {
						id,
						document,
						to_front: true,
						select_after_open: false,
					};
					self.dispatch_desktop_wrapper_message(message);
				}
				for (id, document) in self.persistent_data.documents_after_current() {
					let message = DesktopWrapperMessage::LoadDocument {
						id,
						document,
						to_front: false,
						select_after_open: false,
					};
					self.dispatch_desktop_wrapper_message(message);
				}
				if let Some(id) = self.persistent_data.current_document_id() {
					let message = DesktopWrapperMessage::SelectDocument { id };
					self.dispatch_desktop_wrapper_message(message);
				}
			}
			DesktopFrontendMessage::PersistenceWritePreferences { preferences } => {
				self.persistent_data.write_preferences(preferences);
			}
			DesktopFrontendMessage::PersistenceLoadPreferences => {
				if let Some(preferences) = self.persistent_data.load_preferences() {
					let message = DesktopWrapperMessage::LoadPreferences { preferences };
					self.dispatch_desktop_wrapper_message(message);
				}
			}
		}
	}

	fn handle_desktop_frontend_messages(&mut self, messages: Vec<DesktopFrontendMessage>) {
		for message in messages {
			self.handle_desktop_frontend_message(message);
		}
	}

	fn dispatch_desktop_wrapper_message(&mut self, message: DesktopWrapperMessage) {
		let responses = self.desktop_wrapper.dispatch(message);
		self.handle_desktop_frontend_messages(responses);
	}

	fn send_or_queue_web_message(&mut self, message: Vec<u8>) {
		if self.web_communication_initialized {
			self.cef_context.send_web_message(message);
		} else {
			self.web_communication_startup_buffer.push(message);
		}
	}
}

impl ApplicationHandler<CustomEvent> for WinitApp {
	fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
		// Set a timeout in case we miss any cef schedule requests
		let timeout = Instant::now() + Duration::from_millis(10);
		let wait_until = timeout.min(self.cef_schedule.unwrap_or(timeout));
		if let Some(schedule) = self.cef_schedule
			&& schedule < Instant::now()
		{
			self.cef_schedule = None;
			// Poll cef message loop multiple times to avoid message loop starvation
			for _ in 0..CEF_MESSAGE_LOOP_MAX_ITERATIONS {
				self.cef_context.work();
			}
		}
		if let Some(window) = &self.window.as_ref() {
			window.request_redraw();
		}

		event_loop.set_control_flow(ControlFlow::WaitUntil(wait_until));
	}

	fn resumed(&mut self, event_loop: &ActiveEventLoop) {
		let mut window = Window::default_attributes()
			.with_title(APP_NAME)
			.with_min_inner_size(winit::dpi::LogicalSize::new(400, 300))
			.with_inner_size(winit::dpi::LogicalSize::new(1200, 800))
			// .with_decorations(false)
			.with_resizable(true);

		#[cfg(target_os = "linux")]
		{
			use crate::consts::APP_ID;
			use winit::platform::wayland::ActiveEventLoopExtWayland;

			window = if event_loop.is_wayland() {
				winit::platform::wayland::WindowAttributesExtWayland::with_name(window, APP_ID, "")
			} else {
				winit::platform::x11::WindowAttributesExtX11::with_name(window, APP_ID, APP_NAME)
			}
		}

		let window = event_loop.create_window(window).unwrap();

		#[cfg(target_os = "windows")]
		{
			self.chrome = Some(hybrid_chrome::install(&window, 36));
		}

		let window = Arc::new(window);
		let graphics_state = GraphicsState::new(window.clone(), self.wgpu_context.clone());

		self.window = Some(window);
		self.graphics_state = Some(graphics_state);

		tracing::info!("Winit window created and ready");

		self.desktop_wrapper.init(self.wgpu_context.clone());

		#[cfg(target_os = "windows")]
		let platform = Platform::Windows;
		#[cfg(target_os = "macos")]
		let platform = Platform::Mac;
		#[cfg(target_os = "linux")]
		let platform = Platform::Linux;
		self.dispatch_desktop_wrapper_message(DesktopWrapperMessage::UpdatePlatform(platform));
	}

	fn user_event(&mut self, event_loop: &ActiveEventLoop, event: CustomEvent) {
		match event {
			CustomEvent::WebCommunicationInitialized => {
				self.web_communication_initialized = true;
				for message in self.web_communication_startup_buffer.drain(..) {
					self.cef_context.send_web_message(message);
				}
			}
			CustomEvent::DesktopWrapperMessage(message) => self.dispatch_desktop_wrapper_message(message),
			CustomEvent::NodeGraphExecutionResult(result) => match result {
				NodeGraphExecutionResult::HasRun(texture) => {
					self.dispatch_desktop_wrapper_message(DesktopWrapperMessage::PollNodeGraphEvaluation);
					if let Some(texture) = texture
						&& let Some(graphics_state) = self.graphics_state.as_mut()
						&& let Some(window) = self.window.as_ref()
					{
						graphics_state.bind_viewport_texture(texture);
						window.request_redraw();
					}
				}
				NodeGraphExecutionResult::NotRun => {}
			},
			CustomEvent::UiUpdate(texture) => {
				if let Some(graphics_state) = self.graphics_state.as_mut() {
					graphics_state.resize(texture.width(), texture.height());
					graphics_state.bind_ui_texture(texture);
					let elapsed = self.last_ui_update.elapsed().as_secs_f32();
					self.last_ui_update = Instant::now();
					if elapsed < 0.5 {
						self.avg_frame_time = (self.avg_frame_time * 3. + elapsed) / 4.;
					}
				}
				if let Some(window) = &self.window {
					window.request_redraw();
				}
			}
			CustomEvent::ScheduleBrowserWork(instant) => {
				if instant <= Instant::now() {
					self.cef_context.work();
				} else {
					self.cef_schedule = Some(instant);
				}
			}
			CustomEvent::CloseWindow => {
				// TODO: Implement graceful shutdown

				tracing::info!("Exiting main event loop");
				event_loop.exit();
			}
		}
	}

	fn window_event(&mut self, event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
		self.cef_context.handle_window_event(&event);

		match event {
			WindowEvent::CloseRequested => {
				let _ = self.event_loop_proxy.send_event(CustomEvent::CloseWindow);
			}
			WindowEvent::Resized(PhysicalSize { width, height }) => {
				let _ = self.window_size_sender.send(WindowSize::new(width as usize, height as usize));
				self.cef_context.notify_of_resize();
			}
			WindowEvent::RedrawRequested => {
				let Some(ref mut graphics_state) = self.graphics_state else { return };
				// Only rerender once we have a new UI texture to display
				if let Some(window) = &self.window {
					match graphics_state.render(window.as_ref()) {
						Ok(_) => {}
						Err(wgpu::SurfaceError::Lost) => {
							tracing::warn!("lost surface");
						}
						Err(wgpu::SurfaceError::OutOfMemory) => {
							event_loop.exit();
						}
						Err(e) => tracing::error!("{:?}", e),
					}
					let _ = self.start_render_sender.try_send(());
				}
			}
			// Currently not supported on wayland see https://github.com/rust-windowing/winit/issues/1881
			WindowEvent::DroppedFile(path) => {
				match std::fs::read(&path) {
					Ok(content) => {
						let message = DesktopWrapperMessage::OpenFile { path, content };
						let _ = self.event_loop_proxy.send_event(CustomEvent::DesktopWrapperMessage(message));
					}
					Err(e) => {
						tracing::error!("Failed to read dropped file {}: {}", path.display(), e);
						return;
					}
				};
			}
			_ => {}
		}

		// Notify cef of possible input events
		self.cef_context.work();
	}
}

#[cfg(target_os = "windows")]
mod hybrid_chrome {
	use std::{
		collections::HashMap,
		mem::{MaybeUninit, size_of, transmute},
		ptr::{null, null_mut},
		sync::{Mutex, OnceLock},
	};

	use wgpu::rwh::{HasWindowHandle, RawWindowHandle};
	use winit::window::Window;

	use windows::{
		Win32::{
			Foundation::*,
			Graphics::{
				Dwm::*,
				Gdi::{GetMonitorInfoW, HBRUSH, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
			},
			System::{LibraryLoader::GetModuleHandleW, SystemInformation::*},
			UI::{
				Controls::MARGINS,
				HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
				WindowsAndMessaging::*,
			},
		},
		core::PCWSTR,
	};

	/// Keep this alive while installed; Drop restores the original WndProc and destroys helper band.
	pub struct HybridChromeHandle {
		hwnd: HWND,
	}
	impl Drop for HybridChromeHandle {
		fn drop(&mut self) {
			let _ = unsafe { uninstall_impl(self.hwnd) };
		}
	}

	/// Install borderless (NCCALCSIZE) + hit-testing. `caption_height_px` is the
	/// draggable band you’ll draw into at the top of your **client** area.
	pub fn install(window: &Window, caption_height_px: i32) -> HybridChromeHandle {
		install_with_options(
			window,
			Options {
				caption_height_px,
				enable_dark_caption: true,
				backdrop: Some(1), // 1=Mica (Win11), 2=Acrylic, 3=Tabbed; None to skip
			},
		)
	}

	pub struct Options {
		pub caption_height_px: i32,
		pub enable_dark_caption: bool,
		pub backdrop: Option<i32>,
	}

	pub fn install_with_options(window: &Window, opts: Options) -> HybridChromeHandle {
		let hwnd = hwnd_from_winit(window);

		unsafe {
			// Optional: dark caption + system backdrop (no-op on unsupported builds)
			if opts.enable_dark_caption {
				let on: i32 = 1;
				let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, size_of::<i32>() as u32);
			}
			if let Some(kind) = opts.backdrop {
				let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &kind as *const _ as _, size_of::<i32>() as u32);
			}

			install_impl(hwnd, opts.caption_height_px);

			let mut top_glass: u32 = 1;
			let got = DwmGetWindowAttribute(hwnd, DWMWA_VISIBLE_FRAME_BORDER_THICKNESS, &mut top_glass as *mut _ as *mut _, size_of::<u32>() as u32);
			let margins = MARGINS {
				cxLeftWidth: 0,
				cxRightWidth: 0,
				cyBottomHeight: 0,
				cyTopHeight: if got.is_ok() { top_glass as i32 } else { 1 },
			};
			let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
		}

		unsafe {
			// Remove system caption but keep thickframe (snap/shadow) and control boxes if you want them.
			let mut style = GetWindowLongPtrW(hwnd, GWL_STYLE) as usize;
			style &= !(WS_CAPTION.0 as usize);
			style |= (WS_THICKFRAME.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0) as usize;
			SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);

			// Re-evaluate the frame
			SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER);
		}

		HybridChromeHandle { hwnd }
	}

	fn hwnd_from_winit(window: &Window) -> HWND {
		let handle = window.window_handle().expect("no window handle").as_raw();
		match handle {
			RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut std::ffi::c_void),
			_ => panic!("Not a Win32 window"),
		}
	}

	// ===== helper "resize band" window (option #2) =====
	const RESIZE_BAND_THICKNESS: i32 = 8;

	static HELPER_CLASS_ATOM: OnceLock<u16> = OnceLock::new();
	unsafe fn ensure_helper_class() -> u16 {
		*HELPER_CLASS_ATOM.get_or_init(|| {
			let class_name: Vec<u16> = "HybridChromeResizeBand\0".encode_utf16().collect();
			let wc = WNDCLASSW {
				style: CS_HREDRAW | CS_VREDRAW,
				lpfnWndProc: Some(helper_wndproc),
				hInstance: GetModuleHandleW(None).unwrap().into(),
				hIcon: HICON::default(),
				hCursor: LoadCursorW(HINSTANCE(null_mut()), IDC_ARROW).unwrap(),
				hbrBackground: HBRUSH::default(), // no paint (we handle WM_ERASEBKGND)
				lpszClassName: PCWSTR(class_name.as_ptr()),
				..Default::default()
			};
			RegisterClassW(&wc)
		})
	}

	#[derive(Clone, Copy)]
	struct HelperData {
		owner: HWND,
	}

	// Store both state and helper HWND per owner.
	struct State {
		prev_wndproc: isize,
		caption_height_px: i32,
		helper_hwnd: HWND,
	}

	// SAFETY: HWND is only used on the main thread and not shared across threads.
	unsafe impl Send for State {}
	unsafe impl Sync for State {}

	static STATE: OnceLock<Mutex<HashMap<isize, State>>> = OnceLock::new();
	fn state_map() -> &'static Mutex<HashMap<isize, State>> {
		STATE.get_or_init(|| Mutex::new(HashMap::new()))
	}

	unsafe fn install_impl(hwnd: HWND, caption_height_px: i32) {
		// Create helper band window first (so we can position it right away)
		let _atom = ensure_helper_class();
		let ex = WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW; // non-activating, no taskbar
		let style = WS_POPUP; // top-level popup so it can extend outside owner
		let helper = CreateWindowExW(
			ex,
			PCWSTR("HybridChromeResizeBand\0".encode_utf16().collect::<Vec<_>>().as_ptr()),
			PCWSTR::null(),
			style,
			0,
			0,
			0,
			0,
			None,
			None,
			HINSTANCE(null_mut()),
			Some(&HelperData { owner: hwnd } as *const _ as _),
		);

		let Ok(helper) = helper else {
			panic!("CreateWindowExW for resize band failed");
		};

		// Subclass owner
		let prev = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as isize);
		if prev == 0 {
			DestroyWindow(helper);
			panic!("SetWindowLongPtrW failed");
		}
		state_map().lock().unwrap().insert(
			hwnd.0 as isize,
			State {
				prev_wndproc: prev,
				caption_height_px,
				helper_hwnd: helper,
			},
		);

		// Position helper now and show it (no activation)
		position_helper(hwnd, helper);
		ShowWindow(helper, SW_SHOWNOACTIVATE);
	}

	unsafe fn uninstall_impl(hwnd: HWND) {
		if let Some(state) = state_map().lock().unwrap().remove(&(hwnd.0 as isize)) {
			let _ = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, state.prev_wndproc);
			if state.helper_hwnd.0 != null_mut() {
				DestroyWindow(state.helper_hwnd);
			}
		}
	}

	unsafe fn position_helper(owner: HWND, helper: HWND) {
		let mut r = RECT::default();
		GetWindowRect(owner, &mut r);

		// Expand by thickness on all sides
		let x = r.left - RESIZE_BAND_THICKNESS;
		let y = r.top - RESIZE_BAND_THICKNESS;
		let w = (r.right - r.left) + RESIZE_BAND_THICKNESS * 2;
		let h = (r.bottom - r.top) + RESIZE_BAND_THICKNESS * 2;

		// Keep helper above owner (but not topmost globally)
		SetWindowPos(
			helper,
			owner, // insert after owner -> ends up just above it
			x,
			y,
			w,
			h,
			SWP_NOACTIVATE,
		);
	}

	#[repr(C)]
	struct NCCALCSIZE_PARAMS {
		rgrc: [RECT; 3],
		lppos: *mut windows::Win32::UI::WindowsAndMessaging::WINDOWPOS,
	}

	unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
		match msg {
			WM_NCCALCSIZE => {
				if wparam.0 != 0 {
					return LRESULT(0);
				}
			}

			// Keep helper synced with owner moves/resizes/shows/hides.
			WM_MOVE | WM_MOVING | WM_SIZE | WM_SIZING | WM_WINDOWPOSCHANGED | WM_SHOWWINDOW => {
				if let Some(st) = state_map().lock().unwrap().get(&(hwnd.0 as isize)) {
					if msg == WM_SHOWWINDOW {
						if wparam.0 == 0 {
							ShowWindow(st.helper_hwnd, SW_HIDE);
						} else {
							ShowWindow(st.helper_hwnd, SW_SHOWNOACTIVATE);
						}
					}
					position_helper(hwnd, st.helper_hwnd);
				}
			}

			WM_DESTROY => {
				// Owner being destroyed—clean up helper.
				if let Some(st) = state_map().lock().unwrap().get(&(hwnd.0 as isize)) {
					if st.helper_hwnd.0 != null_mut() {
						DestroyWindow(st.helper_hwnd);
					}
				}
			}

			// Optional: draggable caption area inside client
			WM_NCHITTEST => {
				if let Some(st) = state_map().lock().unwrap().get(&(hwnd.0 as isize)) {
					let sx = (lparam.0 & 0xFFFF) as i16 as i32;
					let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;

					let mut wr = RECT::default();
					GetWindowRect(hwnd, &mut wr);
					let px = sx - wr.left;
					let py = sy - wr.top;

					let ww = wr.right - wr.left;
					let frame_x = 4;
					let frame_y = 4;

					let on_left = px < frame_x;
					let on_right = px >= ww - frame_x;
					let on_top = py < frame_y;
					let on_bottom = py >= (wr.bottom - wr.top) - frame_y;

					if on_top && on_left {
						return LRESULT(HTTOPLEFT as isize);
					}
					if on_top && on_right {
						return LRESULT(HTTOPRIGHT as isize);
					}
					if on_bottom && on_left {
						return LRESULT(HTBOTTOMLEFT as isize);
					}
					if on_bottom && on_right {
						return LRESULT(HTBOTTOMRIGHT as isize);
					}
					if on_top {
						return LRESULT(HTTOP as isize);
					}
					if on_left {
						return LRESULT(HTLEFT as isize);
					}
					if on_right {
						return LRESULT(HTRIGHT as isize);
					}
					if on_bottom {
						return LRESULT(HTBOTTOM as isize);
					}

					// Caption drag band inside client (avoid buttons if you draw them)
					if py >= 0 && py < st.caption_height_px {
						return LRESULT(HTCAPTION as isize);
					}
					return LRESULT(HTCLIENT as isize);
				}
			}

			_ => {}
		}

		let prev = state_map().lock().unwrap().get(&(hwnd.0 as isize)).map(|s| s.prev_wndproc).unwrap_or(0);
		if prev != 0 {
			return CallWindowProcW(transmute(prev), hwnd, msg, wparam, lparam);
		}
		DefWindowProcW(hwnd, msg, wparam, lparam)
	}

	// ===== helper window proc =====
	unsafe extern "system" fn helper_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
		match msg {
			WM_NCCREATE => {
				// Stash owner HWND in GWLP_USERDATA
				let cs = &*(lparam.0 as *const CREATESTRUCTW);
				SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
				return LRESULT(1);
			}
			WM_ERASEBKGND => {
				// Don’t draw anything = visually invisible
				return LRESULT(1);
			}
			WM_NCHITTEST => {
				let data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const HelperData;
				let owner = if !data.is_null() { (*data).owner } else { HWND::default() };

				let sx = (lparam.0 & 0xFFFF) as i16 as i32;
				let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;

				let mut r = RECT::default();
				GetWindowRect(hwnd, &mut r);

				// Which edge of the helper band are we on?
				let on_left = sx < r.left + RESIZE_BAND_THICKNESS;
				let on_right = sx >= r.right - RESIZE_BAND_THICKNESS;
				let on_top = sy < r.top + RESIZE_BAND_THICKNESS;
				let on_bottom = sy >= r.bottom - RESIZE_BAND_THICKNESS;

				// Corners first
				if on_top && on_left {
					return LRESULT(HTTOPLEFT as isize);
				}
				if on_top && on_right {
					return LRESULT(HTTOPRIGHT as isize);
				}
				if on_bottom && on_left {
					return LRESULT(HTBOTTOMLEFT as isize);
				}
				if on_bottom && on_right {
					return LRESULT(HTBOTTOMRIGHT as isize);
				}

				if on_top {
					return LRESULT(HTTOP as isize);
				}
				if on_left {
					return LRESULT(HTLEFT as isize);
				}
				if on_right {
					return LRESULT(HTRIGHT as isize);
				}
				if on_bottom {
					return LRESULT(HTBOTTOM as isize);
				}

				// Otherwise, let clicks fall through (treat as nowhere)
				return LRESULT(HTTRANSPARENT as isize);
			}
			WM_MOUSEACTIVATE => {
				// Never activate on click
				return LRESULT(MA_NOACTIVATE as isize);
			}
			_ => {}
		}
		DefWindowProcW(hwnd, msg, wparam, lparam)
	}
}
