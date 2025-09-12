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
			self.chrome = Some(hybrid_chrome::install(&window));
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
	use std::collections::HashMap;
	use std::ffi::c_void;
	use std::mem::{size_of, transmute};
	use std::ptr::null_mut;
	use std::sync::{Mutex, OnceLock};

	use wgpu::rwh::{HasWindowHandle, RawWindowHandle};
	use winit::window::Window;

	use windows::Win32::Foundation::*;
	use windows::Win32::Graphics::{Dwm::*, Gdi::HBRUSH};
	use windows::Win32::System::LibraryLoader::GetModuleHandleW;
	use windows::Win32::UI::Controls::MARGINS;
	use windows::Win32::UI::WindowsAndMessaging::*;
	use windows::core::PCWSTR;

	pub struct HybridChromeHandle {
		hwnd: HWND,
	}
	impl Drop for HybridChromeHandle {
		fn drop(&mut self) {
			let _ = unsafe { uninstall_impl(self.hwnd) };
		}
	}

	pub fn install(window: &Window) -> HybridChromeHandle {
		install_with_options(
			window,
			Options {
				enable_dark_caption: true,
				backdrop: Some(1),
			},
		)
	}

	pub struct Options {
		pub enable_dark_caption: bool,
		pub backdrop: Option<i32>,
	}

	pub fn install_with_options(window: &Window, opts: Options) -> HybridChromeHandle {
		let hwnd = hwnd_from_winit(window);

		if opts.enable_dark_caption {
			let on: i32 = 1;
			let _ = unsafe { DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, size_of::<i32>() as u32) };
		}
		if let Some(kind) = opts.backdrop {
			let _ = unsafe { DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &kind as *const _ as _, size_of::<i32>() as u32) };
		}

		unsafe { install_impl(hwnd) };

		let mut style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
		style &= !(WS_CAPTION.0);
		style |= (WS_THICKFRAME.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0) as u32;
		unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize) };
		let _ = unsafe { SetWindowPos(hwnd, HWND::default(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER) };

		let mut top_glass: u32 = 1;
		let got = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_VISIBLE_FRAME_BORDER_THICKNESS, &mut top_glass as *mut _ as *mut _, size_of::<u32>() as u32) };
		let margins = MARGINS {
			cxLeftWidth: 0,
			cxRightWidth: 0,
			cyBottomHeight: 0,
			cyTopHeight: if got.is_ok() { top_glass as i32 } else { 1 },
		};
		let _ = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };

		let mut style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as usize;
		style &= !(WS_CAPTION.0 as usize);
		style |= (WS_THICKFRAME.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0) as usize;
		unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize) };

		let _ = unsafe { SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER) };

		HybridChromeHandle { hwnd }
	}

	fn hwnd_from_winit(window: &Window) -> HWND {
		let handle = window.window_handle().expect("no window handle").as_raw();
		match handle {
			RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut std::ffi::c_void),
			_ => panic!("Not a Win32 window"),
		}
	}

	const RESIZE_BAND_THICKNESS: i32 = 8;

	static HELPER_CLASS_ATOM: OnceLock<u16> = OnceLock::new();
	unsafe fn ensure_helper_class() {
		let _ = *HELPER_CLASS_ATOM.get_or_init(|| {
			let class_name: Vec<u16> = "HybridChromeResizeBand\0".encode_utf16().collect();
			let wc = WNDCLASSW {
				style: CS_HREDRAW | CS_VREDRAW,
				lpfnWndProc: Some(helper_wndproc),
				hInstance: unsafe { GetModuleHandleW(None).unwrap().into() },
				hIcon: HICON::default(),
				hCursor: unsafe { LoadCursorW(HINSTANCE(null_mut()), IDC_ARROW).unwrap() },
				hbrBackground: HBRUSH::default(),
				lpszClassName: PCWSTR(class_name.as_ptr()),
				..Default::default()
			};
			unsafe { RegisterClassW(&wc) }
		});
	}

	#[derive(Clone, Copy)]
	struct HelperData {
		owner: HWND,
	}

	struct State {
		prev_wndproc: isize,
		helper_hwnd: HWND,
	}

	unsafe impl Send for State {}
	unsafe impl Sync for State {}

	static STATE: OnceLock<Mutex<HashMap<isize, State>>> = OnceLock::new();
	fn state_map() -> &'static Mutex<HashMap<isize, State>> {
		STATE.get_or_init(|| Mutex::new(HashMap::new()))
	}

	unsafe fn install_impl(hwnd: HWND) {
		unsafe { ensure_helper_class() };
		let ex = WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
		let style = WS_POPUP;
		let init = HelperData { owner: hwnd };
		let helper = unsafe {
			CreateWindowExW(
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
				Some(&init as *const _ as _),
			)
		};

		let Ok(helper) = helper else {
			panic!("CreateWindowExW for resize band failed");
		};

		let prev = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as isize) };
		if prev == 0 {
			let _ = unsafe { DestroyWindow(helper) };
			panic!("SetWindowLongPtrW failed");
		}
		state_map().lock().unwrap().insert(
			hwnd.0 as isize,
			State {
				prev_wndproc: prev,
				helper_hwnd: helper,
			},
		);

		unsafe { position_helper(hwnd, helper) };
		let _ = unsafe { ShowWindow(helper, SW_SHOWNOACTIVATE) };
	}

	unsafe fn uninstall_impl(hwnd: HWND) {
		if let Some(state) = state_map().lock().unwrap().remove(&(hwnd.0 as isize)) {
			let _ = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, state.prev_wndproc) };
			if state.helper_hwnd.0 != null_mut() {
				let _ = unsafe { DestroyWindow(state.helper_hwnd) };
			}
		}
	}

	unsafe fn position_helper(owner: HWND, helper: HWND) {
		let mut r = RECT::default();
		let _ = unsafe { GetWindowRect(owner, &mut r) };

		let x = r.left - RESIZE_BAND_THICKNESS;
		let y = r.top - RESIZE_BAND_THICKNESS;
		let w = (r.right - r.left) + RESIZE_BAND_THICKNESS * 2;
		let h = (r.bottom - r.top) + RESIZE_BAND_THICKNESS * 2;

		let _ = unsafe { SetWindowPos(helper, owner, x, y, w, h, SWP_NOACTIVATE) };
	}

	unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
		match msg {
			WM_NCCALCSIZE => {
				if wparam.0 != 0 {
					return LRESULT(0);
				}
			}

			WM_MOVE | WM_MOVING | WM_SIZE | WM_SIZING | WM_WINDOWPOSCHANGED | WM_SHOWWINDOW => {
				if let Some(st) = state_map().lock().unwrap().get(&(hwnd.0 as isize)) {
					if msg == WM_SHOWWINDOW {
						if wparam.0 == 0 {
							unsafe {
								let _ = ShowWindow(st.helper_hwnd, SW_HIDE);
							};
						} else {
							unsafe {
								let _ = ShowWindow(st.helper_hwnd, SW_SHOWNOACTIVATE);
							};
						}
					}
					unsafe { position_helper(hwnd, st.helper_hwnd) };
				}
			}

			WM_DESTROY => {
				if let Some(st) = state_map().lock().unwrap().get(&(hwnd.0 as isize)) {
					if st.helper_hwnd.0 != null_mut() {
						unsafe {
							let _ = DestroyWindow(st.helper_hwnd);
						};
					}
				}
			}

			_ => {}
		}

		let prev = state_map().lock().unwrap().get(&(hwnd.0 as isize)).map(|s| s.prev_wndproc).unwrap_or(0);
		if prev != 0 {
			return unsafe { CallWindowProcW(transmute(prev), hwnd, msg, wparam, lparam) };
		}
		unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
	}

	unsafe extern "system" fn helper_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
		match msg {
			WM_NCCREATE => {
				let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
				let init = unsafe { &*(cs.lpCreateParams as *const HelperData) };
				unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, init.owner.0 as isize) };
				return LRESULT(1);
			}
			WM_ERASEBKGND => return LRESULT(1),

			WM_NCHITTEST => {
				let sx = (lparam.0 & 0xFFFF) as i16 as u32;
				let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as u32;
				let ht = unsafe { calculate_hit(hwnd, sx, sy) };
				return LRESULT(ht as isize);
			}

			WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN | WM_NCMBUTTONDOWN => {
				let owner_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut c_void;
				let owner = HWND(owner_ptr);
				if unsafe { IsWindow(owner).as_bool() } {
					let sx = (lparam.0 & 0xFFFF) as i16 as u32;
					let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as u32;
					let ht = unsafe { calculate_hit(hwnd, sx, sy) };

					let Some(wmsz) = hit_to_resize_direction(ht) else {
						return LRESULT(0);
					};

					let _ = unsafe { SetForegroundWindow(owner) };

					let _ = unsafe { PostMessageW(owner, WM_SYSCOMMAND, WPARAM((SC_SIZE + wmsz) as usize), lparam) };
					return LRESULT(0);
				}
				return LRESULT(HTTRANSPARENT as isize);
			}

			WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
			_ => {}
		}
		unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
	}

	fn hit_to_resize_direction(ht: u32) -> Option<u32> {
		match ht {
			HTLEFT => Some(WMSZ_LEFT),
			HTRIGHT => Some(WMSZ_RIGHT),
			HTTOP => Some(WMSZ_TOP),
			HTBOTTOM => Some(WMSZ_BOTTOM),
			HTTOPLEFT => Some(WMSZ_TOPLEFT),
			HTTOPRIGHT => Some(WMSZ_TOPRIGHT),
			HTBOTTOMLEFT => Some(WMSZ_BOTTOMLEFT),
			HTBOTTOMRIGHT => Some(WMSZ_BOTTOMRIGHT),
			_ => None,
		}
	}

	unsafe fn calculate_hit(helper: HWND, x: u32, y: u32) -> u32 {
		let mut r = RECT::default();
		let _ = unsafe { GetWindowRect(helper, &mut r) };

		let on_left = x < (r.left + RESIZE_BAND_THICKNESS) as u32;
		let on_right = x >= (r.right - RESIZE_BAND_THICKNESS) as u32;
		let on_top = y < (r.top + RESIZE_BAND_THICKNESS) as u32;
		let on_bottom = y >= (r.bottom - RESIZE_BAND_THICKNESS) as u32;

		if on_top && on_left {
			return HTTOPLEFT;
		}
		if on_top && on_right {
			return HTTOPRIGHT;
		}
		if on_bottom && on_left {
			return HTBOTTOMLEFT;
		}
		if on_bottom && on_right {
			return HTBOTTOMRIGHT;
		}
		if on_top {
			return HTTOP;
		}
		if on_left {
			return HTLEFT;
		}
		if on_right {
			return HTRIGHT;
		}
		if on_bottom {
			return HTBOTTOM;
		}
		HTTRANSPARENT as u32
	}
}
