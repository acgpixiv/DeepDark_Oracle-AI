//! 磁盘物理信息（3D 星图「盘符 = 恒星」的体积与引力依据）。
//!
//! 为什么单独一个模块：星图需要的是**物理容量**，而 `db::drive_stats()`
//! 给的是「索引里有多少字节」。两者语义完全不同 —— 一块 2TB 的盘如果只
//! 索引了 30GB，按索引量算出来的恒星会小得看不出差别，用户要的"容量大的
//! 恒星更大"就落空了。所以容量必须问操作系统。
//!
//! 为什么不用新依赖：`sysinfo` / `sysinfo` 之类的 crate 会往安装包里塞
//! 一堆用不到的平台代码；这里只有三个 Win32 导出，而 `kernel32` 本来就在
//! build.rs 的链接组里（见 build.rs 的 `--start-group` 列表），零新增体积。

use serde::Serialize;

/// 单个磁盘卷的信息。字段全部用"普通字符串 + 字节数"，前端不用再换算。
#[derive(Serialize, Clone, Debug, Default)]
pub struct DriveInfo {
    /// 盘符，形如 `D:`。
    pub mount: String,
    /// 盘符字母，`D`。
    pub letter: String,
    /// 卷标（用户给盘起的名字），空串表示没设置。
    pub label: String,
    /// 文件系统名，`NTFS` / `exFAT`。
    pub fs: String,
    /// 总容量（字节）。
    pub total_bytes: u64,
    /// 可用空间（字节）。
    pub free_bytes: u64,
    /// 是否可移动盘（U 盘、移动硬盘、光驱）。
    pub removable: bool,
}

impl DriveInfo {
    /// 已用比例 0~1，前端拿来画"星环进度"。
    pub fn used_ratio(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        let used = self.total_bytes.saturating_sub(self.free_bytes);
        (used as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
    }
}

#[cfg(windows)]
mod win {
    /// Win32 的 `GetDriveTypeW` 返回值。
    pub const DRIVE_REMOVABLE: u32 = 2;
    /// `DRIVE_FIXED`：固定盘。目前只用于文档说明（判定走"非可移动即固定"），
    /// 保留常量是为了后续区分网络盘/光驱时不用再翻一次 Win32 文档。
    #[allow(dead_code)]
    pub const DRIVE_FIXED: u32 = 3;
    pub const DRIVE_REMOTE: u32 = 4;
    pub const DRIVE_CDROM: u32 = 5;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_available: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
        pub fn GetDriveTypeW(root: *const u16) -> u32;
        pub fn GetVolumeInformationW(
            root: *const u16,
            volume_name: *mut u16,
            volume_name_size: u32,
            serial: *mut u32,
            max_component: *mut u32,
            fs_flags: *mut u32,
            fs_name: *mut u16,
            fs_name_size: u32,
        ) -> i32;
    }
}

/// 枚举本机所有已挂载的盘符及其物理容量。
///
/// 只列出**能读到容量**的卷：光驱空仓、断开的网络盘会返回 0，
/// 这类卷在星图里没有意义（恒星半径会是 0），直接跳过。
pub fn volumes() -> Vec<DriveInfo> {
    #[cfg(windows)]
    {
        let mut out = Vec::new();
        for letter in ('A'..='Z').map(|c| c as u8 as char) {
            let root = format!("{letter}:\\");
            if let Some(info) = probe(&root) {
                out.push(info);
            }
        }
        out.sort_by_key(|d| std::cmp::Reverse(d.total_bytes));
        out
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 只保证能编过：星图在桌面端才有意义。
        vec![DriveInfo {
            mount: "/".into(),
            letter: "/".into(),
            label: String::new(),
            fs: String::new(),
            total_bytes: 0,
            free_bytes: 0,
            removable: false,
        }]
    }
}

#[cfg(windows)]
fn probe(root: &str) -> Option<DriveInfo> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = std::ffi::OsStr::new(root).encode_wide().chain(std::iter::once(0)).collect();

    // 卷类型先判：光驱空仓时 GetDiskFreeSpaceExW 也会失败，先过滤掉更省事。
    let kind = unsafe { win::GetDriveTypeW(wide.as_ptr()) };
    if kind == win::DRIVE_CDROM || kind == win::DRIVE_REMOTE {
        return None;
    }
    if kind == 0 || kind == 1 {
        // 0 = 未知，1 = 无根目录（盘符不存在）
        return None;
    }

    let (mut free_avail, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe {
        win::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_avail,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 || total == 0 {
        return None;
    }

    // 卷标 / 文件系统：拿不到不影响使用，静默留空。
    let mut label_buf = [0u16; 64];
    let mut fs_buf = [0u16; 32];
    let ok_info = unsafe {
        win::GetVolumeInformationW(
            wide.as_ptr(),
            label_buf.as_mut_ptr(),
            label_buf.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_buf.as_mut_ptr(),
            fs_buf.len() as u32,
        )
    };
    let (label, fs) = if ok_info != 0 {
        (utf16_to_string(&label_buf), utf16_to_string(&fs_buf))
    } else {
        (String::new(), String::new())
    };

    Some(DriveInfo {
        mount: root.to_string(),
        letter: root.chars().next().unwrap_or('?').to_string(),
        label,
        fs,
        total_bytes: total,
        free_bytes: free_avail,
        removable: kind == win::DRIVE_REMOVABLE,
    })
}

#[cfg(windows)]
fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 容量 → 恒星半径（世界坐标单位）。
///
/// 立方根而不是线性：10 TB 不该比 100 GB 大 100 倍（那样小盘只剩一个像素）。
/// 立方根把 100 倍容量压成 4.6 倍半径，既看得出差异，又不会失控 —— 正是
/// 需求表里「baseRadius + pow(bytes/GB, 1/3) * 0.5」的意图，只是常数按
/// 星图的实际世界尺度放大到了几十这个量级。
pub fn star_radius_of(total_bytes: u64) -> f32 {
    let gb = total_bytes as f64 / (1024f64 * 1024f64 * 1024f64);
    // 必须与前端 engine3d.ts::starRadiusOf 完全一致，否则同一块盘
    // 在"已索引"与"未索引"两条路径下大小会跳。
    // 20 + cbrt(GB)*8.5：16GB=41、1TB=106、8TB=190 —— 与 3D 场景
    // 150~300 的轨道尺度匹配（旧公式 1.8TB 算出 770，会铺满整个视口）。
    (20.0 + gb.cbrt() * 8.5) as f32
}

/// 容量 → 引力强度 0~1（恒星越大引力越强）。
///
/// 与半径用同一个立方根基底，但归一化到 [0.12, 1]：最小的盘也还有一点
/// 引力（不然它会被行星"甩开"），最大的盘刚好是 1。
pub fn gravity_of(total_bytes: u64) -> f32 {
    let gb = total_bytes as f64 / (1024f64 * 1024f64 * 1024f64);
    let raw = gb.cbrt() * 61.0;
    let norm = raw / (raw + 120.0);
    (0.12 + norm * 0.88) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_radius_grows_sublinearly() {
        let a = star_radius_of(500 * 1024 * 1024 * 1024);
        let b = star_radius_of(1000 * 1024 * 1024 * 1024);
        let c = star_radius_of(10_000 * 1024 * 1024 * 1024);
        assert!(a < b && b < c);
        // 2 倍容量 ≠ 2 倍半径（立方根压缩）。
        assert!((b - a) < a * 0.6);
        // 20 倍容量也不该超过 5 倍半径。
        assert!(c < a * 5.0);
        // 场景尺度护栏：任何单盘的恒星半径都必须远小于相机距离（1000），
        // 否则 billboard 会铺满视口 —— 这是 2026-11-04 「整屏褐色矩形」的根因。
        for g in [16u64, 1024, 8192, 100_000] {
            let r = star_radius_of(g * 1024 * 1024 * 1024);
            assert!(r < 320.0, "star radius {r} too large for {g} GB");
        }
    }

    #[test]
    fn gravity_is_bounded() {
        for gb in [16u64, 256, 1024, 8192, 100_000] {
            let g = gravity_of(gb * 1024 * 1024 * 1024);
            assert!(g >= 0.12 && g <= 1.0, "gravity {g} out of range for {gb} GB");
        }
        assert!(gravity_of(8 * 1024 * 1024 * 1024 * 1024) > gravity_of(64 * 1024 * 1024 * 1024));
    }
}
