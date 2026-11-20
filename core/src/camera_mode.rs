//! Switching a camera control between automatic and manual on Windows. The
//! capture library sets values keeping the mode a control has, so a focus
//! or exposure in automatic mode would ignore a value; this sets the mode
//! through DirectShow's control interfaces before the camera is opened.
//! Design: docs/evidence/image-input.md.

use windows::core::Interface;
use windows::Win32::Media::DirectShow::{
    CameraControlProperty, CameraControl_Exposure, CameraControl_Flags_Auto,
    CameraControl_Flags_Manual, CameraControl_Focus, CameraControl_Iris, CameraControl_Pan,
    CameraControl_Roll, CameraControl_Tilt, CameraControl_Zoom, IAMCameraControl, IAMVideoProcAmp,
    VideoProcAmpProperty, VideoProcAmp_BacklightCompensation, VideoProcAmp_Brightness,
    VideoProcAmp_Contrast, VideoProcAmp_Flags_Auto, VideoProcAmp_Flags_Manual, VideoProcAmp_Gain,
    VideoProcAmp_Gamma, VideoProcAmp_Hue, VideoProcAmp_Saturation, VideoProcAmp_Sharpness,
    VideoProcAmp_WhiteBalance,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, MFCreateAttributes, MFEnumDeviceSources,
    MFShutdown, MFStartup, MFSTARTUP_NOSOCKET, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

/// Which interface holds a control, and its property there.
enum Property {
    Camera(CameraControlProperty),
    Picture(VideoProcAmpProperty),
}

/// The property of a control named as `camera` names them.
fn property(name: &str) -> Option<Property> {
    Some(match name {
        "pan" => Property::Camera(CameraControl_Pan),
        "tilt" => Property::Camera(CameraControl_Tilt),
        "roll" => Property::Camera(CameraControl_Roll),
        "zoom" => Property::Camera(CameraControl_Zoom),
        "exposure" => Property::Camera(CameraControl_Exposure),
        "iris" => Property::Camera(CameraControl_Iris),
        "focus" => Property::Camera(CameraControl_Focus),
        "brightness" => Property::Picture(VideoProcAmp_Brightness),
        "contrast" => Property::Picture(VideoProcAmp_Contrast),
        "hue" => Property::Picture(VideoProcAmp_Hue),
        "saturation" => Property::Picture(VideoProcAmp_Saturation),
        "sharpness" => Property::Picture(VideoProcAmp_Sharpness),
        "gamma" => Property::Picture(VideoProcAmp_Gamma),
        "white_balance" => Property::Picture(VideoProcAmp_WhiteBalance),
        "backlight_compensation" => Property::Picture(VideoProcAmp_BacklightCompensation),
        "gain" => Property::Picture(VideoProcAmp_Gain),
        _ => return None,
    })
}

/// Whether a control has an automatic mode this module can switch.
pub(crate) fn has_mode(name: &str) -> bool {
    property(name).is_some()
}

/// Sets the controls `modes` names (control, automatic) of camera `index`
/// to automatic or manual, keeping their values.
pub(crate) fn set_modes(index: u32, modes: &[(String, bool)]) -> Result<(), String> {
    if modes.is_empty() {
        return Ok(());
    }
    for (name, _) in modes {
        if property(name).is_none() {
            return Err(format!("{name} has no automatic mode to switch"));
        }
    }
    // SAFETY: Media Foundation and COM are used as their documentation
    // describes: started before use, the activation list freed with
    // CoTaskMemFree, and the media source shut down before it is released.
    unsafe {
        // A thread already set up for COM keeps its mode; that is fine.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
            .map_err(|error| format!("cannot start Media Foundation: {error}"))?;
        let result = with_source(index, |source| {
            for (name, automatic) in modes {
                set_mode(source, property(name).expect("checked above"), *automatic).map_err(
                    |error| format!("cannot set {name} to {}: {error}", mode(*automatic)),
                )?;
            }
            Ok(())
        });
        let _ = MFShutdown();
        result
    }
}

fn mode(automatic: bool) -> &'static str {
    if automatic {
        "automatic"
    } else {
        "manual"
    }
}

/// Runs `work` with the media source of camera `index`.
unsafe fn with_source(
    index: u32,
    work: impl FnOnce(&IMFMediaSource) -> Result<(), String>,
) -> Result<(), String> {
    let mut attributes: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attributes, 1).map_err(|error| error.to_string())?;
    let attributes = attributes.ok_or("cannot create the device attributes")?;
    attributes
        .SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
        .map_err(|error| error.to_string())?;
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFEnumDeviceSources(&attributes, &mut list, &mut count)
        .map_err(|error| format!("cannot list cameras: {error}"))?;
    let devices: Vec<Option<IMFActivate>> = if list.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts_mut(list, count as usize)
            .iter_mut()
            .map(Option::take)
            .collect()
    };
    if !list.is_null() {
        CoTaskMemFree(Some(list.cast()));
    }
    let activate = devices
        .get(index as usize)
        .and_then(Option::as_ref)
        .ok_or_else(|| format!("no camera {index}"))?;
    let source: IMFMediaSource = activate
        .ActivateObject()
        .map_err(|error| format!("cannot open camera {index}: {error}"))?;
    let result = work(&source);
    let _ = source.Shutdown();
    result
}

unsafe fn set_mode(
    source: &IMFMediaSource,
    property: Property,
    automatic: bool,
) -> windows::core::Result<()> {
    let (mut value, mut flags) = (0i32, 0i32);
    match property {
        Property::Camera(property) => {
            let control: IAMCameraControl = source.cast()?;
            control.Get(property.0, &mut value, &mut flags)?;
            let flag = if automatic {
                CameraControl_Flags_Auto
            } else {
                CameraControl_Flags_Manual
            };
            control.Set(property.0, value, flag.0)
        }
        Property::Picture(property) => {
            let control: IAMVideoProcAmp = source.cast()?;
            control.Get(property.0, &mut value, &mut flags)?;
            let flag = if automatic {
                VideoProcAmp_Flags_Auto
            } else {
                VideoProcAmp_Flags_Manual
            };
            control.Set(property.0, value, flag.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_with_a_mode_are_known_by_name() {
        assert!(matches!(property("focus"), Some(Property::Camera(_))));
        assert!(matches!(
            property("white_balance"),
            Some(Property::Picture(_))
        ));
        assert!(property("exposure,_auto").is_none());
        assert_eq!(
            set_modes(0, &[("frame_rate".into(), true)]),
            Err("frame_rate has no automatic mode to switch".into())
        );
    }
}
