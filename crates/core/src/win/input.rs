//! Mouse acceleration of the running session (`SystemParametersInfoW`).
//!
//! The values Windows stores in `HKCU\Control Panel\Mouse` are read only at sign-in; these
//! functions read and set the copy the running session uses.

use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETMOUSE, SPI_SETMOUSE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

use crate::Result;

/// The running session's mouse acceleration as `[threshold1, threshold2, speed]`
/// (`SPI_GETMOUSE`). Read-only.
pub fn mouse_acceleration() -> Result<[i32; 3]> {
    let mut params = [0i32; 3];
    // SAFETY: SPI_GETMOUSE writes three ints into the buffer, which holds exactly three and
    // outlives the call.
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSE,
            0,
            Some(params.as_mut_ptr().cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
    }
    Ok(params)
}

/// Sets the running session's mouse acceleration (`SPI_SETMOUSE`) without writing the user
/// profile and without broadcasting WM_SETTINGCHANGE: the registry already holds the values
/// (written and journaled by the safety layer), and Windows reads them at sign-in.
pub fn set_mouse_acceleration(params: [i32; 3]) -> Result<()> {
    let mut params = params;
    // SAFETY: SPI_SETMOUSE reads three ints from the buffer, which holds exactly three and
    // outlives the call; flags 0 neither update the user profile nor broadcast.
    unsafe {
        SystemParametersInfoW(
            SPI_SETMOUSE,
            0,
            Some(params.as_mut_ptr().cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_mouse_acceleration_is_readable() {
        let [threshold1, threshold2, speed] = mouse_acceleration().unwrap();
        assert!((0..=2).contains(&speed), "{speed}");
        assert!(
            threshold1 >= 0 && threshold2 >= 0,
            "{threshold1} {threshold2}"
        );
    }
}
