#[cfg(target_os = "windows")]
mod windows;

pub(crate) enum NativeWindowHandle {
	#[cfg(target_os = "windows")]
	#[expect(private_interfaces, dead_code)]
	Windows(windows::WindowsNativeWindowHandle),
	None,
}

pub(crate) fn setup(window: &winit::window::Window) -> NativeWindowHandle {
	#[cfg(target_os = "windows")]
	return NativeWindowHandle::Windows(windows::install(window));

	#[allow(unreachable_code)]
	NativeWindowHandle::None
}
