//! Host metrics behind the menubar's System Stats submenu.
//!
//! Public API only: Mach per-CPU tick counters (efficiency/performance split via
//! the `hw.perflevel` sysctls), the IOKit accelerator performance dictionary for
//! GPU utilisation and in-use memory, `host_statistics64` for the memory
//! breakdown, `getloadavg`, and `kern.boottime`. Power draw, core clocks and die
//! temperature would need private frameworks and are deliberately out of scope —
//! the coarse thermal pressure level stands in for temperature.
//!
//! The rest of this app reads host state by spawning `sysctl` and `vm_stat`,
//! which is fine once per model load. It is not fine for a submenu that refreshes
//! while it is open, and there is no shell equivalent for per-cluster CPU usage at
//! all, so this path is FFI.
//!
//! Sampling only happens while the submenu is open. CPU usage is a delta of
//! cumulative counters, so the first reading after a long gap still yields a
//! valid average over that window rather than a spike.

use std::ffi::CString;
use std::time::{SystemTime, UNIX_EPOCH};

type NaturalT = u32;
type IntegerT = i32;
type MachPortT = u32;
type KernReturnT = i32;

const KERN_SUCCESS: KernReturnT = 0;
const PROCESSOR_CPU_LOAD_INFO: i32 = 2;
const HOST_VM_INFO64: i32 = 4;
const CPU_STATE_MAX: usize = 4;
const CPU_STATE_USER: usize = 0;
const CPU_STATE_SYSTEM: usize = 1;
const CPU_STATE_IDLE: usize = 2;
const CPU_STATE_NICE: usize = 3;

/// `mach/vm_statistics.h`. Declared in full because `host_statistics64` is given
/// the struct's size in `integer_t` units and validates it.
#[repr(C, align(8))]
#[derive(Default, Clone, Copy)]
struct VmStatistics64 {
    free_count: NaturalT,
    active_count: NaturalT,
    inactive_count: NaturalT,
    wire_count: NaturalT,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: NaturalT,
    speculative_count: NaturalT,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: NaturalT,
    throttled_count: NaturalT,
    external_page_count: NaturalT,
    internal_page_count: NaturalT,
    total_uncompressed_pages_in_compressor: u64,
}

extern "C" {
    fn mach_host_self() -> MachPortT;
    fn mach_task_self() -> MachPortT;
    fn host_page_size(host: MachPortT, out: *mut usize) -> KernReturnT;
    fn host_statistics64(
        host: MachPortT,
        flavor: i32,
        info: *mut IntegerT,
        count: *mut u32,
    ) -> KernReturnT;
    fn host_processor_info(
        host: MachPortT,
        flavor: i32,
        out_processor_count: *mut NaturalT,
        out_processor_info: *mut *mut IntegerT,
        out_processor_infoCnt: *mut u32,
    ) -> KernReturnT;
    fn vm_deallocate(target: MachPortT, address: usize, size: usize) -> KernReturnT;
}

/// Cumulative busy/total ticks for one logical CPU.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CpuTicks {
    busy: u64,
    total: u64,
}

#[derive(Default, Clone, Copy, serde::Serialize)]
pub struct MemoryBreakdown {
    pub total_bytes: u64,
    pub wired_bytes: u64,
    pub active_bytes: u64,
    pub compressed_bytes: u64,
    /// Derived as total − (wired + active + compressed) so the four rows always
    /// sum to the machine total, which is what makes the legend readable.
    pub free_bytes: u64,
    /// Swap in use. The window used to get this from a `vm_stat` subprocess on a
    /// second timer, which is how it could disagree with the rows above; it is
    /// read here so pressure and breakdown come from one tick.
    pub swap_used_bytes: u64,
}

#[derive(Default, Clone, serde::Serialize)]
pub struct SysSnapshot {
    /// Fractions 0..1, or None while a reading is unavailable.
    pub e_core_usage: Option<f64>,
    pub p_core_usage: Option<f64>,
    pub cpu_total_usage: Option<f64>,
    pub gpu_usage: Option<f64>,
    pub gpu_memory_bytes: Option<u64>,
    pub memory: MemoryBreakdown,
    pub load_average: Vec<f64>,
    pub uptime_seconds: f64,
    pub thermal: String,
}

/// Holds the previous tick reading, since CPU usage only exists as a delta.
#[derive(Default)]
pub struct Sampler {
    previous: Vec<CpuTicks>,
    e_core_count: usize,
    probed_cores: bool,
}

impl Sampler {
    pub fn sample(&mut self) -> SysSnapshot {
        if !self.probed_cores {
            self.e_core_count = efficiency_core_count();
            self.probed_cores = true;
        }
        let mut snapshot = SysSnapshot {
            memory: read_memory(),
            load_average: read_load_average(),
            uptime_seconds: read_uptime_seconds(),
            thermal: read_thermal_pressure().to_string(),
            ..Default::default()
        };
        if let Some(gpu) = read_gpu() {
            snapshot.gpu_usage = gpu.0;
            snapshot.gpu_memory_bytes = gpu.1;
        }
        if let Some(current) = read_cpu_ticks() {
            if let Some((e, p, total)) = cluster_usage(&self.previous, &current, self.e_core_count) {
                snapshot.e_core_usage = Some(e);
                snapshot.p_core_usage = Some(p);
                snapshot.cpu_total_usage = Some(total);
            }
            self.previous = current;
        }
        snapshot
    }
}

/// Average busy fraction per cluster between two readings. The kernel's counters
/// are 32-bit and wrap, so a CPU whose counter shrank contributes nothing rather
/// than a garbage delta; a reading with no usable delta at all yields None.
pub fn cluster_usage(
    previous: &[CpuTicks],
    current: &[CpuTicks],
    e_core_count: usize,
) -> Option<(f64, f64, f64)> {
    if previous.len() != current.len() || current.is_empty() {
        return None;
    }
    let (mut e_busy, mut e_total, mut p_busy, mut p_total) = (0.0, 0.0, 0.0, 0.0);
    for (index, (prev, cur)) in previous.iter().zip(current.iter()).enumerate() {
        if cur.total <= prev.total || cur.busy < prev.busy {
            continue;
        }
        let busy = (cur.busy - prev.busy) as f64;
        let total = (cur.total - prev.total) as f64;
        if index < e_core_count {
            e_busy += busy;
            e_total += total;
        } else {
            p_busy += busy;
            p_total += total;
        }
    }
    let all_total = e_total + p_total;
    if all_total <= 0.0 {
        return None;
    }
    Some((
        if e_total > 0.0 { (e_busy / e_total).min(1.0) } else { 0.0 },
        if p_total > 0.0 { (p_busy / p_total).min(1.0) } else { 0.0 },
        ((e_busy + p_busy) / all_total).min(1.0),
    ))
}

/// Logical-CPU count of the efficiency cluster. Apple Silicon enumerates the E
/// cluster first in Mach's per-CPU arrays, and `hw.perflevel1` is the efficiency
/// level whenever two levels exist. Zero (Intel, or unknown) counts every core
/// as performance, which is the honest answer on a machine without clusters.
fn efficiency_core_count() -> usize {
    if sysctl_u32("hw.nperflevels").unwrap_or(0) < 2 {
        return 0;
    }
    sysctl_u32("hw.perflevel1.logicalcpu").unwrap_or(0) as usize
}

fn read_cpu_ticks() -> Option<Vec<CpuTicks>> {
    let mut cpu_count: NaturalT = 0;
    let mut info: *mut IntegerT = std::ptr::null_mut();
    let mut info_count: u32 = 0;
    // SAFETY: out-params only; the returned array is owned by us and freed below.
    let result = unsafe {
        host_processor_info(
            mach_host_self(),
            PROCESSOR_CPU_LOAD_INFO,
            &mut cpu_count,
            &mut info,
            &mut info_count,
        )
    };
    if result != KERN_SUCCESS || info.is_null() {
        return None;
    }
    let mut ticks = Vec::with_capacity(cpu_count as usize);
    for cpu in 0..cpu_count as usize {
        let base = cpu * CPU_STATE_MAX;
        if base + CPU_STATE_MAX > info_count as usize {
            break;
        }
        // SAFETY: index bounded by info_count just above.
        let at = |state: usize| unsafe { *info.add(base + state) as u32 as u64 };
        let busy = at(CPU_STATE_USER)
            .wrapping_add(at(CPU_STATE_SYSTEM))
            .wrapping_add(at(CPU_STATE_NICE));
        ticks.push(CpuTicks {
            busy,
            total: busy.wrapping_add(at(CPU_STATE_IDLE)),
        });
    }
    // SAFETY: freeing exactly the allocation host_processor_info handed us.
    unsafe {
        vm_deallocate(
            mach_task_self(),
            info as usize,
            info_count as usize * std::mem::size_of::<IntegerT>(),
        );
    }
    Some(ticks)
}

fn read_memory() -> MemoryBreakdown {
    let mut memory = MemoryBreakdown {
        total_bytes: sysctl_u64("hw.memsize").unwrap_or(0),
        ..Default::default()
    };
    let mut stats = VmStatistics64::default();
    let mut count = (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<IntegerT>()) as u32;
    let mut page_size: usize = 0;
    // SAFETY: count is the struct's size in integer_t units, as the call expects.
    let ok = unsafe {
        host_statistics64(
            mach_host_self(),
            HOST_VM_INFO64,
            &mut stats as *mut VmStatistics64 as *mut IntegerT,
            &mut count,
        ) == KERN_SUCCESS
            && host_page_size(mach_host_self(), &mut page_size) == KERN_SUCCESS
    };
    if !ok || page_size == 0 {
        return memory;
    }
    let page = page_size as u64;
    memory.wired_bytes = stats.wire_count as u64 * page;
    memory.active_bytes = stats.active_count as u64 * page;
    memory.compressed_bytes = stats.compressor_page_count as u64 * page;
    let used = memory.wired_bytes + memory.active_bytes + memory.compressed_bytes;
    memory.free_bytes = memory.total_bytes.saturating_sub(used);
    memory.swap_used_bytes = sysctl_raw::<XswUsage>("vm.swapusage")
        .map(|swap| swap.used)
        .unwrap_or(0);
    memory
}

/// `xsw_usage`, the shape `vm.swapusage` hands back. Reading the struct beats
/// parsing `sysctl`'s human text, and it costs no process.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct XswUsage {
    total: u64,
    avail: u64,
    used: u64,
    pagesize: u32,
    encrypted: i32,
}

fn read_load_average() -> Vec<f64> {
    let mut out = [0.0f64; 3];
    // SAFETY: writes exactly three doubles into a three-element array.
    let filled = unsafe { getloadavg(out.as_mut_ptr(), 3) };
    if filled <= 0 {
        return Vec::new();
    }
    out[..filled as usize].to_vec()
}

extern "C" {
    fn getloadavg(loadavg: *mut f64, nelem: i32) -> i32;
}

/// Seconds since boot, from `kern.boottime`.
fn read_uptime_seconds() -> f64 {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Timeval {
        tv_sec: i64,
        tv_usec: i32,
        _pad: i32,
    }
    let Some(raw) = sysctl_raw::<Timeval>("kern.boottime") else {
        return 0.0;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let boot = raw.tv_sec as f64 + raw.tv_usec as f64 / 1e6;
    if now > boot { now - boot } else { 0.0 }
}

/// Coarse thermal pressure, the same signal Activity Monitor surfaces. The real
/// die temperature needs a private framework, so this is the honest ceiling of
/// what a sandboxed app can say about heat.
fn read_thermal_pressure() -> &'static str {
    match sysctl_u32("machdep.xcpm.cpu_thermal_level") {
        Some(0) | None => "Nominal",
        Some(level) if level < 40 => "Fair",
        Some(level) if level < 80 => "Serious",
        Some(_) => "Critical",
    }
}

// ---- sysctl helpers -------------------------------------------------------

fn sysctl_raw<T: Default + Copy>(name: &str) -> Option<T> {
    extern "C" {
        fn sysctlbyname(
            name: *const i8,
            oldp: *mut std::ffi::c_void,
            oldlenp: *mut usize,
            newp: *const std::ffi::c_void,
            newlen: usize,
        ) -> i32;
    }
    let key = CString::new(name).ok()?;
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>();
    // SAFETY: buffer and its length describe the same T.
    let ok = unsafe {
        sysctlbyname(
            key.as_ptr(),
            &mut value as *mut T as *mut std::ffi::c_void,
            &mut size,
            std::ptr::null(),
            0,
        ) == 0
    };
    if ok && size == std::mem::size_of::<T>() { Some(value) } else { None }
}

fn sysctl_u32(name: &str) -> Option<u32> {
    sysctl_raw::<u32>(name)
}

fn sysctl_u64(name: &str) -> Option<u64> {
    sysctl_raw::<u64>(name)
}

// ---- GPU ------------------------------------------------------------------

/// Reads the accelerator's `PerformanceStatistics` dictionary from the public
/// IOKit registry — no entitlements. Apple Silicon exposes one AGX service
/// conforming to `IOAccelerator` carrying "Device Utilization %" and
/// "In use system memory".
fn read_gpu() -> Option<(Option<f64>, Option<u64>)> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const i8) -> *mut std::ffi::c_void;
        fn IOServiceGetMatchingServices(
            main_port: MachPortT,
            matching: *mut std::ffi::c_void,
            existing: *mut u32,
        ) -> KernReturnT;
        fn IOIteratorNext(iterator: u32) -> u32;
        fn IOObjectRelease(object: u32) -> KernReturnT;
        fn IORegistryEntryCreateCFProperties(
            entry: u32,
            properties: *mut *const std::ffi::c_void,
            allocator: *const std::ffi::c_void,
            options: u32,
        ) -> KernReturnT;
    }

    let class = CString::new("IOAccelerator").ok()?;
    let mut iterator: u32 = 0;
    // SAFETY: IOServiceGetMatchingServices consumes the matching dictionary.
    // kIOMainPortDefault is 0, which selects the default port.
    let matched = unsafe {
        IOServiceGetMatchingServices(0, IOServiceMatching(class.as_ptr()), &mut iterator)
            == KERN_SUCCESS
    };
    if !matched {
        return None;
    }
    let mut result = None;
    loop {
        // SAFETY: iterator is live until released below.
        let service = unsafe { IOIteratorNext(iterator) };
        if service == 0 {
            break;
        }
        let mut raw: *const std::ffi::c_void = std::ptr::null();
        // SAFETY: out-param; on success we own the dictionary.
        let ok = unsafe {
            IORegistryEntryCreateCFProperties(service, &mut raw, std::ptr::null(), 0)
                == KERN_SUCCESS
        } && !raw.is_null();
        if ok {
            // SAFETY: wrapping a +1 reference we own.
            let properties: CFDictionary = unsafe { CFDictionary::wrap_under_create_rule(raw as _) };
            // The dictionary is heterogeneous, so values come back untyped and
            // are borrowed at +0 from their owner.
            let lookup = |dictionary: &CFDictionary, key: &str| -> Option<CFType> {
                let key = CFString::new(key);
                let value = dictionary.find(key.as_concrete_TypeRef() as *const std::ffi::c_void)?;
                // SAFETY: value is owned by the dictionary, which outlives this.
                Some(unsafe { CFType::wrap_under_get_rule(*value as _) })
            };
            if let Some(statistics) = lookup(&properties, "PerformanceStatistics")
                .and_then(|value| value.downcast::<CFDictionary>())
            {
                let number = |key: &str| {
                    lookup(&statistics, key)
                        .and_then(|value| value.downcast::<CFNumber>())
                        .and_then(|value| value.to_f64())
                };
                let usage = number("Device Utilization %").map(|p| (p / 100.0).clamp(0.0, 1.0));
                let memory = number("In use system memory").map(|b| b.max(0.0) as u64);
                if usage.is_some() || memory.is_some() {
                    result = Some((usage, memory));
                }
            }
        }
        // SAFETY: releasing the service this iteration produced.
        unsafe { IOObjectRelease(service) };
        if result.is_some() {
            break;
        }
    }
    // SAFETY: releasing the iterator obtained above.
    unsafe { IOObjectRelease(iterator) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks(busy: u64, total: u64) -> CpuTicks {
        CpuTicks { busy, total }
    }

    #[test]
    fn cluster_usage_splits_at_the_efficiency_boundary() {
        // Two E cores fully busy, two P cores idle.
        let previous = vec![ticks(0, 0); 4];
        let current = vec![ticks(100, 100), ticks(100, 100), ticks(0, 100), ticks(0, 100)];
        let (e, p, total) = cluster_usage(&previous, &current, 2).expect("usable delta");
        assert_eq!(e, 1.0);
        assert_eq!(p, 0.0);
        assert_eq!(total, 0.5);
    }

    #[test]
    fn a_wrapped_counter_is_skipped_rather_than_read_as_a_huge_delta() {
        // The kernel's counters are 32-bit; CPU 0 wrapped, CPU 1 is usable.
        let previous = vec![ticks(500, 900), ticks(0, 0)];
        let current = vec![ticks(10, 20), ticks(50, 100)];
        let (_, p, total) = cluster_usage(&previous, &current, 0).expect("one usable delta");
        assert_eq!(p, 0.5);
        assert_eq!(total, 0.5);
    }

    #[test]
    fn no_usable_delta_yields_nothing_instead_of_zero() {
        // Identical readings: reporting 0% here would be a lie about an idle CPU.
        let same = vec![ticks(10, 100)];
        assert!(cluster_usage(&same, &same, 0).is_none());
        assert!(cluster_usage(&[], &[], 0).is_none());
        assert!(cluster_usage(&same, &vec![ticks(1, 2); 2], 0).is_none());
    }

    #[test]
    fn every_core_counts_as_performance_without_clusters() {
        let previous = vec![ticks(0, 0); 2];
        let current = vec![ticks(50, 100), ticks(50, 100)];
        let (e, p, _) = cluster_usage(&previous, &current, 0).expect("usable delta");
        assert_eq!(e, 0.0);
        assert_eq!(p, 0.5);
    }

    #[test]
    fn the_memory_rows_sum_to_the_machine_total() {
        let memory = read_memory();
        assert!(memory.total_bytes > 0, "hw.memsize must be readable");
        assert_eq!(
            memory.wired_bytes + memory.active_bytes + memory.compressed_bytes + memory.free_bytes,
            memory.total_bytes
        );
    }

    #[test]
    fn the_host_answers_at_all() {
        let mut sampler = Sampler::default();
        let first = sampler.sample();
        assert!(first.uptime_seconds > 0.0, "kern.boottime must be readable");
        assert!(!first.load_average.is_empty(), "getloadavg must answer");
        // The first sample has no previous ticks to diff against.
        assert!(first.cpu_total_usage.is_none());
        std::thread::sleep(std::time::Duration::from_millis(120));
        let second = sampler.sample();
        let cpu = second.cpu_total_usage.expect("a second reading gives a delta");
        assert!((0.0..=1.0).contains(&cpu));
    }

    /// `vm.swapusage` comes back as a struct, so a wrong layout would not fail —
    /// it would hand back a plausible-looking number from a neighbouring field.
    /// Closing the three fields is not enough to catch that, because swapping
    /// used with free still adds up; so this reads what `sysctl` prints as text
    /// and insists the struct agrees with it.
    #[test]
    fn swap_in_use_is_the_used_field_and_not_its_neighbour() {
        let swap = sysctl_raw::<XswUsage>("vm.swapusage").expect("vm.swapusage must answer");
        assert!(swap.pagesize.is_power_of_two(), "page size looks misaligned");
        assert_eq!(swap.used + swap.avail, swap.total, "the three fields must close");

        let printed = std::process::Command::new("sysctl")
            .arg("-n")
            .arg("vm.swapusage")
            .output()
            .expect("sysctl must run");
        let text = String::from_utf8_lossy(&printed.stdout);
        let mib: f64 = text
            .split("used =")
            .nth(1)
            .and_then(|rest| rest.trim().split('M').next())
            .and_then(|value| value.trim().parse().ok())
            .expect("sysctl prints used swap in MiB");
        // Text is rounded to two decimals of a MiB, so compare within a MiB.
        let struct_mib = swap.used as f64 / (1024.0 * 1024.0);
        assert!(
            (struct_mib - mib).abs() < 1.0,
            "struct says {struct_mib:.2} MiB used, sysctl printed {mib:.2} MiB"
        );
        assert_eq!(Sampler::default().sample().memory.swap_used_bytes, swap.used);
    }
}
