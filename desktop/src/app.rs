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
					#[cfg(target_os = "windows")]
					unsafe {
						ring::sync_ring_to_main()
					};
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
		unsafe {
			use wgpu::rwh::HasWindowHandle;
			use wgpu::rwh::RawWindowHandle;
			use windows::Win32::Foundation::*;
			use windows::Win32::Graphics::Dwm::{DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DwmSetWindowAttribute};
			use windows::Win32::UI::Controls::MARGINS;
			use windows::Win32::UI::WindowsAndMessaging::*;

			let hwnd = match window.window_handle().unwrap().as_raw() {
				RawWindowHandle::Win32(h) => h,
				_ => panic!("Not using Win32 window handle on Windows"),
			};

			let hwnd = match handle.as_raw() {
				raw_window_handle::RawWindowHandle::Win32(h) => windows::Win32::Foundation::HWND(h.hwnd.get() as isize),
				_ => return,
			};
			ring::create_ring(hwnd);
			ring::sync_ring_to_main();
		}

		//TODO   // configure_window_decorations(&window);

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
				#[cfg(target_os = "windows")]
				unsafe {
					ring::sync_ring_to_main()
				};
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
mod ring {
	use windows::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
	use windows::core::w;

	// thickness of the *external* draggable ring (logical px)
	const RING: i32 = 8;

	static mut MAIN: HWND = HWND(std::ptr::null_mut());
	static mut RINGHWND: HWND = HWND(std::ptr::null_mut());

	pub unsafe fn create_ring(owner: HWND) {
		MAIN = owner;

		let hinst = HINSTANCE(GetModuleHandleW(None).unwrap().0);

		// Register a tiny window class for the ring
		let class = w!("RingBorderWnd");
		let wc = WNDCLASSW {
			lpfnWndProc: Some(wndproc),
			hInstance: hinst,
			lpszClassName: class,
			hCursor: LoadCursorW(None, IDC_ARROW).unwrap(),
			..Default::default()
		};
		RegisterClassW(&wc);

		// Create owned, layered, no-activate popup that we can make invisible but hit-testable.
		let ex = WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
		let style = WS_POPUP;

		RINGHWND = CreateWindowExW(
			ex, class, None, style, 0, 0, 0, 0, owner, // owner, not parent
			None, hinst, None,
		)
		.unwrap();

		// Show without activating.
		ShowWindow(RINGHWND, SW_SHOWNA);
	}

	pub unsafe fn sync_ring_to_main() {
		if MAIN.0.is_null() || RINGHWND.0.is_null() {
			return;
		}

		// Get the main window’s outer rect in screen coords
		let mut rc: RECT = Default::default();
		windows::Win32::UI::WindowsAndMessaging::GetWindowRect(MAIN, &mut rc);

		// Expand by RING (DPI aware)
		let scale = dpi_scale(MAIN);
		let g = (RING as f32 * scale) as i32;

		let x = rc.left - g;
		let y = rc.top - g;
		let w = (rc.right - rc.left) + 2 * g;
		let h = (rc.bottom - rc.top) + 2 * g;

		SetWindowPos(RINGHWND, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW);
	}

	extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
		unsafe {
			match msg {
				WM_LBUTTONDOWN => {
					// Convert cursor to ring client coords
					let mut pt = POINT {
						x: GET_X_LPARAM(l.0),
						y: GET_Y_LPARAM(l.0),
					};
					ClientToScreen(hwnd, &mut pt);

					// Figure out which edge/corner ring zone the mouse is in, then
					// ask the MAIN window to start native sizing via WM_SYSCOMMAND/SC_SIZE.
					let edge = edge_from_point(hwnd, pt);
					if let Some(cmd) = edge_to_sc_size(edge) {
						// Start the system sizing loop on the main window
						PostMessageW(MAIN, WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
					}
					return LRESULT(0);
				}
				WM_NCHITTEST => {
					// Make the cursor show the right resize arrows when hovering the ring.
					let screen_x = GET_X_LPARAM(l.0);
					let screen_y = GET_Y_LPARAM(l.0);
					let edge = edge_from_point(hwnd, POINT { x: screen_x, y: screen_y });
					let code = match edge {
						1 => HTLEFT,
						2 => HTRIGHT,
						3 => HTTOP,
						4 => HTBOTTOM,
						5 => HTTOPLEFT,
						6 => HTTOPRIGHT,
						7 => HTBOTTOMLEFT,
						8 => HTBOTTOMRIGHT,
						_ => HTCLIENT,
					};
					return LRESULT(code as isize);
				}
				_ => {}
			}
			DefWindowProcW(hwnd, msg, w, l)
		}
	}

	// Map our edge id to SC_SIZE codes (WMSZ_*) for WM_SYSCOMMAND.
	fn edge_to_sc_size(edge: i32) -> Option<u32> {
		Some(match edge {
			1 => SC_SIZE | WMSZ_LEFT as u32,
			2 => SC_SIZE | WMSZ_RIGHT as u32,
			3 => SC_SIZE | WMSZ_TOP as u32,
			4 => SC_SIZE | WMSZ_BOTTOM as u32,
			5 => SC_SIZE | WMSZ_TOPLEFT as u32,
			6 => SC_SIZE | WMSZ_TOPRIGHT as u32,
			7 => SC_SIZE | WMSZ_BOTTOMLEFT as u32,
			8 => SC_SIZE | WMSZ_BOTTOMRIGHT as u32,
			_ => return None,
		})
	}

	unsafe fn edge_from_point(hwnd: HWND, screen_pt: POINT) -> i32 {
		// Get ring rect
		let mut r: RECT = Default::default();
		windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut r);

		let scale = dpi_scale(MAIN);
		let g = (RING as f32 * scale) as i32;

		// inner rect equals owner's rect (ring minus g on all sides)
		let inner = RECT {
			left: r.left + g,
			top: r.top + g,
			right: r.right - g,
			bottom: r.bottom - g,
		};

		// Which band are we in?
		let left = screen_pt.x < inner.left;
		let right = screen_pt.x >= inner.right;
		let top = screen_pt.y < inner.top;
		let bottom = screen_pt.y >= inner.bottom;

		// corners first
		if top && left {
			return 5;
		}
		if top && right {
			return 6;
		}
		if bottom && left {
			return 7;
		}
		if bottom && right {
			return 8;
		}

		if left {
			return 1;
		}
		if right {
			return 2;
		}
		if top {
			return 3;
		}
		if bottom {
			return 4;
		}
		0
	}

	fn dpi_scale(hwnd: HWND) -> f32 {
		unsafe {
			let hdc = GetDC(hwnd);
			if !hdc.0.is_null() {
				let dpi = GetDeviceCaps(hdc, LOGPIXELSX) as f32;
				ReleaseDC(hwnd, hdc);
				return dpi / 96.0;
			}
		}
		1.0
	}

	// helpers for LPARAM unpack
	const fn GET_X_LPARAM(lp: isize) -> i32 {
		(lp & 0xFFFF) as i16 as i32
	}
	const fn GET_Y_LPARAM(lp: isize) -> i32 {
		((lp >> 16) & 0xFFFF) as i16 as i32
	}
}
