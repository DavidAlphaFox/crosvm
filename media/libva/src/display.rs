// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::CStr;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::anyhow;
use anyhow::Context as AnyhowContext;
use anyhow::Result;
use base::AsRawDescriptor;

use crate::bindings;
use crate::config::Config;
use crate::context::Context;
use crate::status::Status;
use crate::surface::Surface;
use crate::UsageHint;

/// Iterates over existing DRM devices.
///
/// DRM devices can be passed to [`Display::open_drm_display`] in order to create a `Display` on
/// that device.
pub struct DrmDeviceIterator {
    cur_idx: usize,
}

const DRM_NODE_DEFAULT_PREFIX: &str = "/dev/dri/renderD";
const DRM_NUM_NODES: usize = 64;
const DRM_RENDER_NODE_START: usize = 128;

impl DrmDeviceIterator {
    /// Create a new iterator over DRM render nodes on the system.
    pub fn new() -> Self {
        Self {
            cur_idx: DRM_RENDER_NODE_START,
        }
    }
}

impl Iterator for DrmDeviceIterator {
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        match self.cur_idx {
            idx if idx >= DRM_RENDER_NODE_START + DRM_NUM_NODES => None,
            idx => {
                let path = PathBuf::from(format!("{}{}", DRM_NODE_DEFAULT_PREFIX, idx));
                if !path.exists() {
                    None
                } else {
                    self.cur_idx += 1;
                    Some(path)
                }
            }
        }
    }
}

/// A VADisplay opened over DRM.
///
/// A Display is the starting point to using libva. This struct is essentially a safe wrapper over
/// `VADisplay`, from which [`Surface`]s and [`Context`]s can be allocated in order to perform
/// actual work using [`Display::create_surfaces`] and [`Display::create_context`], respectively.
///
/// Although libva offers several ways to create a display, this struct currently only supports
/// opening through DRM. It may be extended to support other display types (X11, Wayland) in the
/// future.
pub struct Display {
    /// Handle to interact with the underlying `VADisplay`.
    handle: bindings::VADisplay,
    /// DRM file that must be kept open while the display is in use.
    #[allow(dead_code)]
    drm_file: File,
}

impl Display {
    /// Opens and initializes a specific DRM `Display`.
    ///
    /// `path` is the path to a DRM device that supports VAAPI, e.g. `/dev/dri/renderD128`.
    pub fn open_drm_display<P: AsRef<Path>>(path: P) -> Result<Rc<Self>> {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .context(format!("failed to open {}", path.as_ref().display()))?;

        // Safe because fd represents a valid file descriptor and the pointer is checked for
        // NULL afterwards.
        let display = unsafe { bindings::vaGetDisplayDRM(file.as_raw_descriptor()) };
        if display.is_null() {
            // The File will close the DRM fd on drop.
            return Err(anyhow!(
                "failed to obtain VA display from DRM device {}",
                path.as_ref().display()
            ));
        }

        let mut major = 0i32;
        let mut minor = 0i32;
        // Safe because we ensure that the display is valid (i.e not NULL) before calling
        // vaInitialize. The File will close the DRM fd on drop.
        Status(unsafe { bindings::vaInitialize(display, &mut major, &mut minor) }).check()?;

        Ok(Rc::new(Self {
            handle: display,
            drm_file: file,
        }))
    }

    /// Opens the first device that succeeds and returns its `Display`.
    ///
    /// If an error occurs on a given device, it is ignored and the next one is tried until one
    /// succeeds or we reach the end of the iterator.
    pub fn open() -> Option<Rc<Self>> {
        let devices = DrmDeviceIterator::new();

        // Try all the DRM devices until one succeeds.
        for device in devices {
            if let Ok(display) = Self::open_drm_display(device) {
                return Some(display);
            }
        }

        None
    }

    /// Returns the handle of this display.
    pub(crate) fn handle(&self) -> bindings::VADisplay {
        self.handle
    }

    /// Queries supported profiles by this display.
    pub fn query_config_profiles(&self) -> Result<Vec<bindings::VAProfile::Type>> {
        // Safe because `self` represents a valid VADisplay.
        let mut max_num_profiles = unsafe { bindings::vaMaxNumProfiles(self.handle) };
        let mut profiles = Vec::with_capacity(max_num_profiles as usize);

        // Safe because `self` represents a valid `VADisplay` and the vector has `max_num_profiles`
        // as capacity.
        Status(unsafe {
            bindings::vaQueryConfigProfiles(
                self.handle,
                profiles.as_mut_ptr(),
                &mut max_num_profiles,
            )
        })
        .check()?;

        // Safe because `profiles` is allocated with a `max_num_profiles` capacity and
        // `vaQueryConfigProfiles` wrote the actual number of profiles to `max_num_entrypoints`.
        unsafe {
            profiles.set_len(max_num_profiles as usize);
        };

        Ok(profiles)
    }

    /// Returns a string describing some aspects of the VA implemenation on the specific hardware
    /// accelerator used by this display.
    ///
    /// The format of the returned string is vendor specific and at the discretion of the
    /// implementer. e.g. for the Intel GMA500 implementation, an example would be: `Intel GMA500 -
    /// 2.0.0.32L.0005`.
    pub fn query_vendor_string(&self) -> std::result::Result<String, &'static str> {
        // Safe because `self` represents a valid VADisplay.
        let vendor_string = unsafe { bindings::vaQueryVendorString(self.handle) };

        if vendor_string.is_null() {
            return Err("vaQueryVendorString() returned NULL");
        }

        // Safe because we check the whether the vendor_String pointer is NULL
        Ok(unsafe { CStr::from_ptr(vendor_string) }
            .to_string_lossy()
            .to_string())
    }

    /// Query supported entrypoints for a given profile.
    pub fn query_config_entrypoints(
        &self,
        profile: bindings::VAProfile::Type,
    ) -> Result<Vec<bindings::VAEntrypoint::Type>> {
        // Safe because `self` represents a valid VADisplay.
        let mut max_num_entrypoints = unsafe { bindings::vaMaxNumEntrypoints(self.handle) };
        let mut entrypoints = Vec::with_capacity(max_num_entrypoints as usize);

        // Safe because `self` represents a valid VADisplay and the vector has `max_num_entrypoints`
        // as capacity.
        Status(unsafe {
            bindings::vaQueryConfigEntrypoints(
                self.handle,
                profile,
                entrypoints.as_mut_ptr(),
                &mut max_num_entrypoints,
            )
        })
        .check()?;

        // Safe because `entrypoints` is allocated with a `max_num_entrypoints` capacity, and
        // `vaQueryConfigEntrypoints` wrote the actual number of entrypoints to
        // `max_num_entrypoints`
        unsafe {
            entrypoints.set_len(max_num_entrypoints as usize);
        }

        Ok(entrypoints)
    }

    /// Writes attributes for a given `profile`/`entrypoint` pair into `attributes`.
    ///
    /// Entries of `attributes` must have their `type_` member initialized to the desired attribute
    /// to retrieve.
    pub fn get_config_attributes(
        &self,
        profile: bindings::VAProfile::Type,
        entrypoint: bindings::VAEntrypoint::Type,
        attributes: &mut [bindings::VAConfigAttrib],
    ) -> Result<()> {
        // Safe because `self` represents a valid VADisplay. The slice length is passed to the C
        // function, so it is impossible to write past the end of the slice's storage by mistake.
        Status(unsafe {
            bindings::vaGetConfigAttributes(
                self.handle,
                profile,
                entrypoint,
                attributes.as_mut_ptr(),
                attributes.len() as i32,
            )
        })
        .check()
    }

    /// Creates `Surface`s by wrapping around a `vaCreateSurfaces` call.
    ///
    /// # Arguments
    ///
    /// * `rt_format` - The desired surface format. See `VA_RT_FORMAT_*`
    /// * `va_fourcc` - The desired pixel format (optional). See `VA_FOURCC_*`
    /// * `width` - Width for the create surfaces
    /// * `height` - Height for the created surfaces
    /// * `usage_hint` - Optional hint of intended usage to optimize allocation (e.g. tiling)
    /// * `num_surfaces` - Number of surfaces to create
    pub fn create_surfaces(
        self: &Rc<Self>,
        rt_format: u32,
        va_fourcc: Option<u32>,
        width: u32,
        height: u32,
        usage_hint: Option<UsageHint>,
        num_surfaces: u32,
    ) -> Result<Vec<Surface>> {
        Surface::new(
            Rc::clone(self),
            rt_format,
            va_fourcc,
            width,
            height,
            usage_hint,
            num_surfaces,
        )
    }

    /// Creates a `Context` by wrapping around a `vaCreateContext` call.
    ///
    /// # Arguments
    ///
    /// * `config` - The configuration for the context
    /// * `coded_width` - The coded picture width
    /// * `coded_height` - The coded picture height
    /// * `surfaces` - Optional hint for the amount of surfaces tied to the context
    /// * `progressive` - Whether only progressive frame pictures are present in the sequence
    pub fn create_context(
        self: &Rc<Self>,
        config: &Config,
        coded_width: i32,
        coded_height: i32,
        surfaces: Option<&Vec<Surface>>,
        progressive: bool,
    ) -> Result<Rc<Context>> {
        Context::new(
            Rc::clone(self),
            config,
            coded_width,
            coded_height,
            surfaces,
            progressive,
        )
    }

    /// Creates a `Config` by wrapping around the `vaCreateConfig` call.
    ///
    /// `attrs` describe the attributes to set for this config. A list of the supported attributes
    /// for a given profile/entrypoint pair can be retrieved using
    /// [`Display::get_config_attributes`]. Other attributes will take their default values, and
    /// `attrs` can be empty in order to obtain a default configuration.
    pub fn create_config(
        self: &Rc<Self>,
        attrs: Vec<bindings::VAConfigAttrib>,
        profile: bindings::VAProfile::Type,
        entrypoint: bindings::VAEntrypoint::Type,
    ) -> Result<Config> {
        Config::new(Rc::clone(self), attrs, profile, entrypoint)
    }

    /// Returns available image formats for this display by wrapping around `vaQueryImageFormats`.
    pub fn query_image_formats(self: &Rc<Self>) -> Result<Vec<bindings::VAImageFormat>> {
        // Safe because `self` represents a valid VADisplay.
        let mut num_image_formats = unsafe { bindings::vaMaxNumImageFormats(self.handle) };
        let mut image_formats = Vec::with_capacity(num_image_formats as usize);

        // Safe because `self` represents a valid VADisplay. The `image_formats` vector is properly
        // initialized and a valid size is passed to the C function, so it is impossible to write
        // past the end of their storage by mistake.
        Status(unsafe {
            bindings::vaQueryImageFormats(
                self.handle,
                image_formats.as_mut_ptr(),
                &mut num_image_formats,
            )
        })
        .check()?;

        // Safe because the C function will have written exactly `num_image_format` entries, which
        // is known to be within the vector's capacity.
        unsafe {
            image_formats.set_len(num_image_formats as usize);
        }

        Ok(image_formats)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // Safe because `self` represents a valid VADisplay.
        unsafe {
            bindings::vaTerminate(self.handle);
            // The File will close the DRM fd on drop.
        }
    }
}
