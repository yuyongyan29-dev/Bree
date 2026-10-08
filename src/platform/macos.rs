//! macOS read-only boundary. Every ABI call checks the returned status and size.
//! libproc is not a control capability: no kill, terminate or shell execution exists here.
use super::{AppEvidence, RawProcess};
use crate::model::*;
use libc::{c_char, c_int, c_void};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
use std::ffi::CStr;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const RSS_SOURCE: &str = "libproc PROC_PIDTASKINFO.pti_resident_size (bytes)";
const VM_SOURCE: &str = "Mach host_statistics64(HOST_VM_INFO64)";
const PRESSURE_SOURCE: &str = "sysctl kern.memorystatus_vm_pressure_level (kernel dispatch level)";
static MAIN_INITIALIZED: AtomicBool = AtomicBool::new(false);
static APP_CACHE: OnceLock<Mutex<Vec<AppEvidence>>> = OnceLock::new();

unsafe extern "C" {
    // Public Mach header declaration absent from libc's current macOS bindings.
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

struct HostPort(libc::mach_port_t);
impl HostPort {
    #[allow(deprecated)]
    fn new() -> Self {
        Self(unsafe { libc::mach_host_self() })
    }
}
impl Drop for HostPort {
    fn drop(&mut self) {
        #[allow(deprecated)]
        // SAFETY: this balances exactly one mach_host_self send right.
        unsafe {
            mach_port_deallocate(libc::mach_task_self(), self.0);
        }
    }
}

#[derive(Debug)]
struct ReadError {
    status: Validity,
    message: String,
}
impl ReadError {
    fn os(operation: &str) -> Self {
        let e = std::io::Error::last_os_error();
        let status = match e.raw_os_error() {
            Some(libc::EACCES | libc::EPERM) => Validity::Denied,
            Some(libc::ESRCH) => Validity::Exited,
            Some(libc::ENOENT | libc::ENOTSUP) => Validity::Unsupported,
            _ => Validity::Unknown,
        };
        Self {
            status,
            message: format!("{operation}: {e}"),
        }
    }
    fn invalid(operation: &str) -> Self {
        Self {
            status: Validity::Unknown,
            message: operation.into(),
        }
    }
    fn metric<T>(self, source: &str) -> Metric<T> {
        Metric::unavailable(self.status, source, self.message)
    }
}

pub(crate) struct Backend {
    boot_session: String,
    boot_valid: bool,
    uid: u32,
    total_memory: Metric<u64>,
    page_size: Option<u64>,
    cpu_timebase: Option<(u32, u32)>,
    initialization_note: Option<String>,
}

impl Backend {
    pub fn new() -> Result<Self, String> {
        // NSWorkspace is initialized before handing numeric sampling to a worker.
        if unsafe { libc::pthread_main_np() } != 0 {
            autoreleasepool(|_| {
                let _workspace = NSWorkspace::sharedWorkspace();
            });
            MAIN_INITIALIZED.store(true, Ordering::Release);
            pump_events();
        }
        let (boot_session, boot_valid) = match sysctl_string(c"kern.bootsessionuuid") {
            Ok(value) if !value.is_empty() => (value, true),
            _ => ("unknown-boot".into(), false),
        };
        let total_memory = match sysctl_value::<u64>(c"hw.memsize") {
            Ok(value) if value > 0 => Metric::ok(value, "sysctl hw.memsize"),
            Ok(_) => Metric::unavailable(
                Validity::Unknown,
                "sysctl hw.memsize",
                "Total physical memory is zero; result invalid",
            ),
            Err(e) => e.metric("sysctl hw.memsize"),
        };
        let page_size = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
            value if value > 0 => Some(value as u64),
            _ => None,
        };
        let cpu_timebase = read_cpu_timebase();
        Ok(Self {
            boot_session,
            boot_valid,
            uid: unsafe { libc::geteuid() },
            total_memory,
            page_size,
            cpu_timebase,
            initialization_note: if MAIN_INITIALIZED.load(Ordering::Acquire) {
                None
            } else {
                Some("AppKit was not initialized on the main thread; dynamic app information may lag, and all actions are disabled.".into())
            },
        })
    }

    pub fn current_uid(&self) -> u32 {
        self.uid
    }

    pub fn system_memory(&self) -> SystemMemory {
        let vm = vm_statistics();
        let (used_bytes, compressed_bytes, cached_bytes) = match (&vm, self.page_size) {
            (Ok(stat), Some(page_size)) => {
                // This is the same documented accounting used by sysinfo's macOS backend.
                let internal = u64::from(stat.internal_page_count);
                let purgeable = u64::from(stat.purgeable_count);
                let wired = u64::from(stat.wire_count);
                let compressor = u64::from(stat.compressor_page_count);
                let file_backed = u64::from(stat.external_page_count);
                let used = (internal.saturating_sub(purgeable) + wired + compressor)
                    .checked_mul(page_size);
                let compressed = compressor.checked_mul(page_size);
                let cached = (file_backed + purgeable).checked_mul(page_size);
                (
                    checked_bytes(used, VM_SOURCE),
                    checked_bytes(compressed, VM_SOURCE),
                    checked_bytes(cached, VM_SOURCE),
                )
            }
            (Err(e), _) => {
                let missing = || Metric::unavailable(e.status, VM_SOURCE, &e.message);
                (missing(), missing(), missing())
            }
            (_, None) => {
                let missing = || {
                    Metric::unavailable(
                        Validity::Unknown,
                        "sysconf _SC_PAGESIZE",
                        "Page size is unreadable",
                    )
                };
                (missing(), missing(), missing())
            }
        };
        let swap_used_bytes = match sysctl_value::<libc::xsw_usage>(c"vm.swapusage") {
            Ok(swap) => Metric::ok(swap.xsu_used, "sysctl vm.swapusage.xsu_used (bytes)"),
            Err(e) => e.metric("sysctl vm.swapusage.xsu_used (bytes)"),
        };
        let pressure = match sysctl_value::<u32>(c"kern.memorystatus_vm_pressure_level") {
            Ok(1) => Metric::ok(Pressure::Normal, PRESSURE_SOURCE),
            Ok(2) => Metric::ok(Pressure::Elevated, PRESSURE_SOURCE),
            Ok(4) => Metric::ok(Pressure::High, PRESSURE_SOURCE),
            Ok(value) => Metric::unavailable(
                Validity::Unknown,
                PRESSURE_SOURCE,
                format!("Unrecognized kernel level {value}"),
            ),
            Err(e) => e.metric(PRESSURE_SOURCE),
        };
        SystemMemory { total_bytes: self.total_memory.clone(), used_bytes, compressed_bytes,
            swap_used_bytes, cached_bytes, pressure,
            used_definition: "Used = (internal_page_count - purgeable_count + wire_count + compressor_page_count) × system page size; compressed = compressor_page_count × page size (actual compressed footprint); estimated cache = (external_page_count + purgeable_count) × page size. Based on system VM counters, not process totals; not guaranteed to match Activity Monitor exactly.".into() }
    }

    pub fn processes(&self) -> Result<(Vec<RawProcess>, Vec<String>), String> {
        let pids = enumerate_pids().map_err(|e| e.message)?;
        let mut diagnostics = Vec::new();
        if let Some(note) = &self.initialization_note {
            diagnostics.push(note.clone());
        }
        if !self.boot_valid {
            diagnostics.push(
                "Boot session UUID is unreadable; instance identity downgraded to unknown.".into(),
            );
        }
        let processes = pids
            .into_iter()
            .map(|pid| self.sample_process(pid))
            .collect();
        Ok((processes, diagnostics))
    }

    fn sample_process(&self, pid: u32) -> RawProcess {
        let before = bsd_info(pid);
        let fallback_identity = ProcessIdentity {
            boot_session: self.boot_session.clone(),
            pid,
            start_seconds: None,
            start_microseconds: None,
            status: Validity::Unknown,
        };
        let mut result = RawProcess {
            identity: fallback_identity,
            parent_pid: None,
            uid: None,
            name: format!("PID {pid}"),
            executable_path: None,
            memory_bytes: Metric::unavailable(Validity::Unknown, RSS_SOURCE, "Identity not read"),
            cpu_total_ns: None,
            sampled_at: Instant::now(),
        };
        let before = match before {
            Ok(value) => value,
            Err(e) => {
                result.identity.status = e.status;
                result.memory_bytes = e.metric(RSS_SOURCE);
                return result;
            }
        };
        result.identity = identity(&self.boot_session, self.boot_valid, &before);
        result.parent_pid = Some(before.pbi_ppid);
        result.uid = Some(before.pbi_uid);
        result.name = fixed_string(&before.pbi_name);
        if result.name.is_empty() {
            result.name = fixed_string(&before.pbi_comm);
        }
        if result.name.is_empty() {
            result.name = format!("PID {pid}");
        }
        result.executable_path = executable_path(pid).ok();
        let task = task_info(pid);
        result.sampled_at = Instant::now();
        // A PID can be reused during multiple reads. Discard all numeric values if it changed.
        match bsd_info(pid) {
            Ok(after)
                if same_instance(&before, &after) && result.identity.status == Validity::Ok =>
            {
                match task {
                    Ok(task) => {
                        result.memory_bytes = Metric::ok(task.pti_resident_size, RSS_SOURCE);
                        // XNU fills these with recount_times_mach. They are NOT nanoseconds on ARM.
                        result.cpu_total_ns = task
                            .pti_total_user
                            .checked_add(task.pti_total_system)
                            .and_then(|ticks| {
                                self.cpu_timebase
                                    .and_then(|ratio| ticks_to_ns(ticks, ratio))
                            });
                    }
                    Err(e) => result.memory_bytes = e.metric(RSS_SOURCE),
                }
            }
            Ok(_) => {
                let status = if result.identity.status == Validity::Ok {
                    Validity::Stale
                } else {
                    Validity::Unknown
                };
                result.identity.status = status;
                result.memory_bytes = Metric::unavailable(
                    status,
                    RSS_SOURCE,
                    "Instance identity differs before and after sampling, or its identity cannot be verified",
                );
                result.executable_path = None;
            }
            Err(e) => {
                result.identity.status = e.status;
                result.memory_bytes = e.metric(RSS_SOURCE);
                result.executable_path = None;
            }
        }
        result
    }

    pub fn applications(&self) -> Vec<AppEvidence> {
        if !MAIN_INITIALIZED.load(Ordering::Acquire) {
            return Vec::new();
        }
        if unsafe { libc::pthread_main_np() } != 0 {
            pump_events();
        }
        APP_CACHE
            .get()
            .and_then(|cache| cache.lock().ok().map(|data| data.clone()))
            .unwrap_or_default()
    }
}

pub(crate) fn pump_events() {
    if unsafe { libc::pthread_main_np() } == 0 {
        return;
    }
    autoreleasepool(|_| {
        let deadline = NSDate::dateWithTimeIntervalSinceNow(0.001);
        // NSRunLoop and this constant are used only on the main thread.
        let mode = unsafe { NSDefaultRunLoopMode };
        let _ = NSRunLoop::mainRunLoop().runMode_beforeDate(mode, &deadline);
    });
    if MAIN_INITIALIZED.load(Ordering::Acquire) {
        let apps = read_applications_on_main_thread();
        if let Ok(mut cache) = APP_CACHE.get_or_init(|| Mutex::new(Vec::new())).lock() {
            *cache = apps;
        }
    }
}

fn read_applications_on_main_thread() -> Vec<AppEvidence> {
    // Callers only invoke this after the main-thread run loop turn. No Objective-C
    // object is retained in the cache or passed to a worker.
    debug_assert!(unsafe { libc::pthread_main_np() } != 0);
    let Ok(boot) = sysctl_string(c"kern.bootsessionuuid") else {
        return Vec::new();
    };
    autoreleasepool(|_| {
        let workspace = NSWorkspace::sharedWorkspace();
        let frontmost_pid = workspace
            .frontmostApplication()
            .map(|app| app.processIdentifier());
        let applications = workspace.runningApplications();
        (0..applications.count())
            .filter_map(|index| {
                let app = applications.objectAtIndex(index);
                let pid = app.processIdentifier();
                if pid <= 0 || app.isTerminated() {
                    return None;
                }
                let before = bsd_info(pid as u32).ok()?;
                let leader_identity = identity(&boot, true, &before);
                if leader_identity.status != Validity::Ok {
                    return None;
                }
                let bundle_path = app.bundleURL()?.path()?.to_string();
                let app_path = app.executableURL()?.path()?.to_string();
                let actual_path = executable_path(pid as u32).ok()?;
                if actual_path != app_path
                    || !std::path::Path::new(&actual_path).starts_with(&bundle_path)
                {
                    return None;
                }
                let after = bsd_info(pid as u32).ok()?;
                if !same_instance(&before, &after) {
                    return None;
                }
                let name = app
                    .localizedName()
                    .map(|v| safe_text(&v.to_string()))
                    .unwrap_or_else(|| fixed_string(&before.pbi_name));
                let policy = app.activationPolicy();
                let activation_policy = if policy == NSApplicationActivationPolicy::Regular {
                    ActivationPolicy::Regular
                } else if policy == NSApplicationActivationPolicy::Accessory {
                    ActivationPolicy::Accessory
                } else if policy == NSApplicationActivationPolicy::Prohibited {
                    ActivationPolicy::Prohibited
                } else {
                    ActivationPolicy::Unknown
                };
                Some(AppEvidence {
                    app: Application {
                        bundle_id: app.bundleIdentifier().map(|v| safe_text(&v.to_string())),
                        bundle_path: safe_text(&bundle_path),
                        name,
                        leader_pid: pid as u32,
                        frontmost: frontmost_pid == Some(pid),
                        activation_policy,
                    },
                    leader_identity,
                    executable_path: actual_path,
                    leader_uid: before.pbi_uid,
                })
            })
            .collect()
    })
}

fn checked_bytes(value: Option<u64>, source: &str) -> Metric<u64> {
    match value {
        Some(value) => Metric::ok(value, source),
        None => Metric::unavailable(Validity::Unknown, source, "Memory byte count overflow"),
    }
}

fn ticks_to_ns(ticks: u64, (numer, denom): (u32, u32)) -> Option<u64> {
    if denom == 0 {
        return None;
    }
    u64::try_from(u128::from(ticks) * u128::from(numer) / u128::from(denom)).ok()
}

#[allow(deprecated)]
fn read_cpu_timebase() -> Option<(u32, u32)> {
    // libc deprecates these bindings in favor of a second crate, not the OS API.
    let mut timebase: libc::mach_timebase_info = unsafe { mem::zeroed() };
    let code = unsafe { libc::mach_timebase_info(&mut timebase) };
    if code == libc::KERN_SUCCESS && timebase.numer > 0 && timebase.denom > 0 {
        Some((timebase.numer, timebase.denom))
    } else {
        None
    }
}

fn sysctl_value<T>(name: &CStr) -> Result<T, ReadError> {
    let mut value = MaybeUninit::<T>::zeroed();
    let expected = mem::size_of::<T>();
    let mut length = expected;
    // SAFETY: a valid output allocation and exact length are supplied; no writes requested.
    let code = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut length,
            ptr::null_mut(),
            0,
        )
    };
    if code != 0 {
        return Err(ReadError::os(&name.to_string_lossy()));
    }
    if length != expected {
        return Err(ReadError::invalid(&format!(
            "{} returned structure size {length}, expected {expected}",
            name.to_string_lossy()
        )));
    }
    Ok(unsafe { value.assume_init() })
}

fn sysctl_string(name: &CStr) -> Result<String, ReadError> {
    let mut length = 0_usize;
    let code = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            ptr::null_mut(),
            &mut length,
            ptr::null_mut(),
            0,
        )
    };
    if code != 0 {
        return Err(ReadError::os(&name.to_string_lossy()));
    }
    if length == 0 || length > 4096 {
        return Err(ReadError::invalid("Invalid boot session identity length"));
    }
    let mut value = vec![0_u8; length];
    let code = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut length,
            ptr::null_mut(),
            0,
        )
    };
    if code != 0 {
        return Err(ReadError::os(&name.to_string_lossy()));
    }
    value.truncate(length);
    let end = value.iter().position(|v| *v == 0).unwrap_or(value.len());
    Ok(safe_text(&String::from_utf8_lossy(&value[..end])))
}

fn vm_statistics() -> Result<libc::vm_statistics64, ReadError> {
    let mut stat = MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let host = HostPort::new();
    let code = unsafe {
        libc::host_statistics64(
            host.0,
            libc::HOST_VM_INFO64,
            stat.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if code != libc::KERN_SUCCESS {
        return Err(ReadError::invalid(&format!(
            "host_statistics64 returned Mach error {code}"
        )));
    }
    // Earlier OS releases return an older prefix. All fields used here must be present.
    let required_bytes = mem::offset_of!(libc::vm_statistics64, internal_page_count)
        + mem::size_of::<libc::natural_t>();
    if (count as usize) * mem::size_of::<libc::integer_t>() < required_bytes {
        return Err(ReadError::invalid(
            "The structure returned by host_statistics64 lacks required fields",
        ));
    }
    Ok(unsafe { stat.assume_init() })
}

fn enumerate_pids() -> Result<Vec<u32>, ReadError> {
    let estimated = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    if estimated <= 0 {
        return Err(ReadError::os("proc_listallpids size"));
    }
    let mut capacity = estimated as usize + 128;
    for _ in 0..4 {
        if capacity > 1_000_000 {
            return Err(ReadError::invalid(
                "Process enumeration capacity exceeds the reasonable limit",
            ));
        }
        let mut pids = vec![0_i32; capacity];
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (capacity * mem::size_of::<i32>()) as c_int,
            )
        };
        if count <= 0 {
            return Err(ReadError::os("proc_listallpids"));
        }
        if count as usize >= capacity {
            capacity *= 2;
            continue;
        }
        pids.truncate(count as usize);
        let mut pids: Vec<u32> = pids
            .into_iter()
            .filter(|p| *p >= 0)
            .map(|p| p as u32)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        return Ok(pids);
    }
    Err(ReadError::invalid(
        "The process list kept growing; complete enumeration was not obtained",
    ))
}

fn pid_info<T>(pid: u32, flavor: c_int) -> Result<T, ReadError> {
    let mut value = MaybeUninit::<T>::zeroed();
    let size = mem::size_of::<T>();
    let count = unsafe {
        libc::proc_pidinfo(
            pid as c_int,
            flavor,
            0,
            value.as_mut_ptr().cast::<c_void>(),
            size as c_int,
        )
    };
    if count <= 0 {
        return Err(ReadError::os(&format!("proc_pidinfo({flavor})")));
    }
    if count as usize != size {
        return Err(ReadError::invalid(&format!(
            "proc_pidinfo({flavor}) structure size {count}, expected {size}"
        )));
    }
    Ok(unsafe { value.assume_init() })
}

fn bsd_info(pid: u32) -> Result<libc::proc_bsdinfo, ReadError> {
    let value: libc::proc_bsdinfo = pid_info(pid, libc::PROC_PIDTBSDINFO)?;
    if value.pbi_pid != pid {
        return Err(ReadError::invalid(
            "The PID returned by proc_bsdinfo differs from the requested PID",
        ));
    }
    Ok(value)
}
fn task_info(pid: u32) -> Result<libc::proc_taskinfo, ReadError> {
    pid_info(pid, libc::PROC_PIDTASKINFO)
}

fn identity(boot: &str, boot_valid: bool, value: &libc::proc_bsdinfo) -> ProcessIdentity {
    let valid_start = value.pbi_start_tvsec > 0 && value.pbi_start_tvusec < 1_000_000;
    ProcessIdentity {
        boot_session: boot.into(),
        pid: value.pbi_pid,
        start_seconds: valid_start.then_some(value.pbi_start_tvsec),
        start_microseconds: valid_start.then_some(value.pbi_start_tvusec as u32),
        status: if valid_start && boot_valid {
            Validity::Ok
        } else {
            Validity::Unknown
        },
    }
}

fn same_instance(a: &libc::proc_bsdinfo, b: &libc::proc_bsdinfo) -> bool {
    a.pbi_pid == b.pbi_pid
        && a.pbi_start_tvsec == b.pbi_start_tvsec
        && a.pbi_start_tvusec == b.pbi_start_tvusec
}

fn executable_path(pid: u32) -> Result<String, ReadError> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let count = unsafe {
        libc::proc_pidpath(
            pid as c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if count <= 0 {
        return Err(ReadError::os("proc_pidpath"));
    }
    if count as usize >= buffer.len() {
        return Err(ReadError::invalid("Executable path truncated"));
    }
    let end = buffer.iter().position(|v| *v == 0).unwrap_or(buffer.len());
    if end == buffer.len() {
        return Err(ReadError::invalid("Executable path has no terminator"));
    }
    Ok(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

fn fixed_string(value: &[c_char]) -> String {
    let bytes: Vec<u8> = value
        .iter()
        .take_while(|v| **v != 0)
        .map(|v| *v as u8)
        .collect();
    safe_text(&String::from_utf8_lossy(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_tick_conversion_preserves_arm_ratio_and_checks_overflow() {
        assert_eq!(ticks_to_ns(24_000_000, (125, 3)), Some(1_000_000_000));
        assert_eq!(ticks_to_ns(123, (1, 1)), Some(123));
        assert_eq!(ticks_to_ns(123, (1, 0)), None);
        assert_eq!(ticks_to_ns(u64::MAX, (125, 3)), None);
    }
    #[test]
    fn identity_preserves_microseconds_and_invalidates_missing_boot() {
        let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
        info.pbi_pid = 123;
        info.pbi_start_tvsec = 100;
        info.pbi_start_tvusec = 999_999;
        let id = identity("boot", true, &info);
        assert_eq!(id.start_microseconds, Some(999_999));
        assert_eq!(id.status, Validity::Ok);
        assert_eq!(identity("unknown", false, &info).status, Validity::Unknown);
        info.pbi_start_tvusec = 1_000_000;
        assert_eq!(identity("boot", true, &info).start_microseconds, None);
    }
    #[test]
    fn sysctl_errors_are_missing_and_unknown_sizes_are_rejected() {
        let missing = sysctl_value::<u64>(c"bree.invalid.nonexistent")
            .unwrap_err()
            .metric::<u64>("test");
        assert_eq!(missing.value, None);
        assert!(sysctl_value::<u8>(c"hw.memsize").is_err());
    }
    #[test]
    fn self_rss_and_precise_instance_are_readable() {
        let backend = Backend::new().unwrap();
        let sampled = backend.sample_process(std::process::id());
        assert_eq!(sampled.identity.status, Validity::Ok);
        assert!(sampled.memory_bytes.value.is_some_and(|v| v > 0));
        assert!(sampled.cpu_total_ns.is_some());
        assert!(sampled.executable_path.is_some());
    }
}
