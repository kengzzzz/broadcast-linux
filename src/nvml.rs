//! Whole-GPU figures from the driver's NVML, loaded at run time.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};

type Handle = *mut c_void;
type Call = unsafe extern "C" fn() -> c_int;
type ByUuid = unsafe extern "C" fn(*const c_char, *mut Handle) -> c_int;
type Utilization = unsafe extern "C" fn(Handle, *mut Rates) -> c_int;
type Clock = unsafe extern "C" fn(Handle, c_uint, *mut c_uint) -> c_int;

#[repr(C)]
#[derive(Default)]
struct Rates {
    gpu: c_uint,
    memory: c_uint,
}

const CLOCK_GRAPHICS: c_uint = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Share of the last sample period in which a kernel ran, at whatever clock.
    pub busy_percent: u32,
    /// 0 when the GPU doesn't report it.
    pub clock_mhz: u32,
    pub max_clock_mhz: u32,
}

pub struct Nvml {
    library: *mut c_void,
    shutdown: Call,
    by_uuid: ByUuid,
    utilization: Utilization,
    clock: Clock,
    max_clock: Clock,
}

impl Nvml {
    /// `None` without the NVIDIA driver.
    pub fn open() -> Option<Self> {
        // SAFETY: a NUL-terminated name; Drop closes the handle.
        let library = unsafe {
            libc::dlopen(
                c"libnvidia-ml.so.1".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        if library.is_null() {
            return None;
        }
        let symbol = |name: &CStr| {
            // SAFETY: `library` is a live handle and `name` is NUL-terminated.
            let address = unsafe { libc::dlsym(library, name.as_ptr()) };
            (!address.is_null()).then_some(address)
        };
        // SAFETY: each symbol has the C signature NVML documents for it.
        let loaded = unsafe {
            (|| {
                let init = std::mem::transmute::<*mut c_void, Call>(symbol(c"nvmlInit_v2")?);
                let shutdown = std::mem::transmute::<*mut c_void, Call>(symbol(c"nvmlShutdown")?);
                let by_uuid = std::mem::transmute::<*mut c_void, ByUuid>(symbol(
                    c"nvmlDeviceGetHandleByUUID",
                )?);
                let utilization = std::mem::transmute::<*mut c_void, Utilization>(symbol(
                    c"nvmlDeviceGetUtilizationRates",
                )?);
                let clock =
                    std::mem::transmute::<*mut c_void, Clock>(symbol(c"nvmlDeviceGetClockInfo")?);
                let max_clock = std::mem::transmute::<*mut c_void, Clock>(symbol(
                    c"nvmlDeviceGetMaxClockInfo",
                )?);
                // Built only once init succeeds, so Drop never shuts down or closes twice.
                (init() == 0).then(|| Self {
                    library,
                    shutdown,
                    by_uuid,
                    utilization,
                    clock,
                    max_clock,
                })
            })()
        };
        if loaded.is_none() {
            // SAFETY: nothing from the library is kept.
            unsafe { libc::dlclose(library) };
        }
        loaded
    }

    pub fn sample(&self, uuid: &str) -> Option<Sample> {
        let uuid = CString::new(uuid).ok()?;
        let mut device = std::ptr::null_mut();
        let mut rates = Rates::default();
        let (mut clock, mut max_clock) = (0, 0);
        // SAFETY: NVML is initialised while `self` lives, and every pointer is a valid
        // out-parameter for the call.
        unsafe {
            if (self.by_uuid)(uuid.as_ptr(), &raw mut device) != 0
                || (self.utilization)(device, &raw mut rates) != 0
            {
                return None;
            }
            if (self.clock)(device, CLOCK_GRAPHICS, &raw mut clock) != 0
                || (self.max_clock)(device, CLOCK_GRAPHICS, &raw mut max_clock) != 0
            {
                (clock, max_clock) = (0, 0);
            }
        }
        Some(Sample {
            busy_percent: rates.gpu,
            clock_mhz: clock,
            max_clock_mhz: max_clock,
        })
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: `open` initialised NVML, and nothing calls into it after this.
        unsafe {
            (self.shutdown)();
            libc::dlclose(self.library);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only checks something on a machine with the NVIDIA driver.
    #[test]
    fn samples_only_known_gpus() {
        let Some(nvml) = Nvml::open() else {
            return;
        };
        assert_eq!(
            nvml.sample("GPU-00000000-0000-0000-0000-000000000000"),
            None
        );
        if let Ok(chosen) = crate::gpu::detect(None) {
            let sample = nvml
                .sample(&chosen.gpu.uuid)
                .expect("a sample of the detected GPU");
            assert!(sample.busy_percent <= 100);
        }
    }
}
