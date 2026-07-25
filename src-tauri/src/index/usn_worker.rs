use crate::index::lib as mft_lib;
use crate::index::ntfs_mft;
use crate::index::path_table::PathTable;
use crate::index::usn_types::{UsnCommand, UsnResponse, UsnState, VolumeState};
use crate::search::{FileEntry, SearchResult};
use compact_str::CompactString;
use crossbeam_channel::{Receiver, Sender};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::time::Instant;
use usn_journal_rs::mft::EnumOptions;
use usn_journal_rs::path::PathResolvableEntry;
use usn_journal_rs::volume::Volume;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Ioctl::{FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0};
use windows::Win32::System::IO::DeviceIoControl;

/// Convert NTFS timestamp (100ns intervals since 1601-01-01) to Unix timestamp (seconds since 1970)
const NTFS_EPOCH_DIFF: i64 = 11_644_473_600; // seconds between 1601 and 1970

#[inline]
fn ntfs_time_to_unix(ntfs_time: Option<u64>) -> i64 {
    match ntfs_time {
        Some(t) if t > 0 => (t as i64 / 10_000_000) - NTFS_EPOCH_DIFF,
        _ => 0,
    }
}

/// USN_REASON_MASK_ALL：监控所有变更原因
const USN_REASON_MASK_ALL: u32 = 0xFFFFFFFF;

/// 从 FSCTL_READ_USN_JOURNAL 直接解析的原始 USN 记录
struct RawUsnRecord {
    #[allow(dead_code)]
    usn: i64,
    fid: u64,
    parent_fid: u64,
    reason: u32,
    file_name: OsString,
    file_attributes: u32,
}

/// 实现 PathResolvableEntry，使 RawUsnRecord 可用于 usn-journal-rs 的路径解析
impl usn_journal_rs::path::PathResolvableEntry for RawUsnRecord {
    fn fid(&self) -> u64 {
        self.fid
    }
    fn parent_fid(&self) -> u64 {
        self.parent_fid
    }
    fn file_name(&self) -> &OsString {
        &self.file_name
    }
    fn is_dir(&self) -> bool {
        // FILE_ATTRIBUTE_DIRECTORY = 0x10
        (self.file_attributes & 0x10) != 0
    }
}

impl RawUsnRecord {
    /// 判断是否为隐藏文件（FILE_ATTRIBUTE_HIDDEN = 0x02）
    fn is_hidden(&self) -> bool {
        (self.file_attributes & 0x02) != 0
    }
}

/// 直接调用 FSCTL_READ_USN_JOURNAL 读取 USN 变更记录
///
/// 绕过 usn-journal-rs 的 iter_with_options，原因：
/// 1. iter_with_options 内部用 entry.usn 作为 last_usn，但正确的 last_usn 应为 API 返回的 next-start USN
/// 2. last_usn + 1 不是有效的记录边界，导致 ERROR_INVALID_PARAMETER (0x80070057)
/// 3. next-start USN（buffer 前 8 字节）是 UsnJournalIter 的私有字段，外部无法访问
///
/// 返回 (记录列表, next_start_usn)，next_start_usn 可作为下次调用的 start_usn
fn read_usn_records_direct(
    handle: HANDLE,
    journal_id: u64,
    start_usn: i64,
) -> Result<(Vec<RawUsnRecord>, i64), String> {
    let mut records: Vec<RawUsnRecord> = Vec::new();
    let mut current_start_usn = start_usn;
    // 64KB 缓冲区，单次可读取数百条记录
    let buffer_size: usize = 64 * 1024;
    let mut buffer = vec![0u8; buffer_size];

    loop {
        // 构造 READ_USN_JOURNAL_DATA_V0 输入参数
        let read_data = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: current_start_usn,
            ReasonMask: USN_REASON_MASK_ALL,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            // BytesToWaitFor=0：非阻塞模式，有数据立即返回，无数据返回 ERROR_HANDLE_EOF
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
        };

        let mut bytes_returned: u32 = 0;
        let result = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_READ_USN_JOURNAL,
                Some(&read_data as *const _ as *const _),
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
                buffer.len() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        match result {
            Ok(()) => {
                // buffer 前 8 字节是 next_start_usn（下一次读取应使用的 StartUsn）
                if (bytes_returned as usize) < std::mem::size_of::<i64>() {
                    // 返回数据不足以包含 next_start_usn，视为无更多数据
                    break;
                }

                let next_start_usn = i64::from_le_bytes([
                    buffer[0], buffer[1], buffer[2], buffer[3], buffer[4], buffer[5], buffer[6],
                    buffer[7],
                ]);

                // 解析 USN_RECORD_V2 记录（从 offset=8 开始，跳过 next_start_usn 头部）
                let mut offset = std::mem::size_of::<i64>();
                while offset < bytes_returned as usize {
                    // 读取 RecordLength（前 4 字节）
                    if offset + 4 > bytes_returned as usize {
                        break;
                    }
                    let record_length = u32::from_le_bytes([
                        buffer[offset],
                        buffer[offset + 1],
                        buffer[offset + 2],
                        buffer[offset + 3],
                    ]);

                    if record_length == 0
                        || offset + record_length as usize > bytes_returned as usize
                    {
                        break;
                    }

                    // USN_RECORD_V2 布局：
                    // +0:  RecordLength (u32)
                    // +4:  MajorVersion (u16)
                    // +6:  MinorVersion (u16)
                    // +8:  FileReferenceNumber (u64)
                    // +16: ParentFileReferenceNumber (u64)
                    // +24: Usn (i64)
                    // +32: TimeStamp (i64)
                    // +40: Reason (u32)
                    // +44: SourceInfo (u32)
                    // +48: SecurityId (u32)
                    // +52: FileAttributes (u32)
                    // +56: FileNameLength (u16, 字节数)
                    // +58: FileNameOffset (u16, 从记录起始的偏移)
                    // +60: FileName (UTF-16, 零终止)
                    let fid = u64::from_le_bytes([
                        buffer[offset + 8],
                        buffer[offset + 9],
                        buffer[offset + 10],
                        buffer[offset + 11],
                        buffer[offset + 12],
                        buffer[offset + 13],
                        buffer[offset + 14],
                        buffer[offset + 15],
                    ]);
                    let parent_fid = u64::from_le_bytes([
                        buffer[offset + 16],
                        buffer[offset + 17],
                        buffer[offset + 18],
                        buffer[offset + 19],
                        buffer[offset + 20],
                        buffer[offset + 21],
                        buffer[offset + 22],
                        buffer[offset + 23],
                    ]);
                    let usn = i64::from_le_bytes([
                        buffer[offset + 24],
                        buffer[offset + 25],
                        buffer[offset + 26],
                        buffer[offset + 27],
                        buffer[offset + 28],
                        buffer[offset + 29],
                        buffer[offset + 30],
                        buffer[offset + 31],
                    ]);
                    let reason = u32::from_le_bytes([
                        buffer[offset + 40],
                        buffer[offset + 41],
                        buffer[offset + 42],
                        buffer[offset + 43],
                    ]);
                    let file_attributes = u32::from_le_bytes([
                        buffer[offset + 52],
                        buffer[offset + 53],
                        buffer[offset + 54],
                        buffer[offset + 55],
                    ]);
                    let file_name_length =
                        u16::from_le_bytes([buffer[offset + 56], buffer[offset + 57]]) as usize;
                    let file_name_offset =
                        u16::from_le_bytes([buffer[offset + 58], buffer[offset + 59]]) as usize;

                    // 读取文件名（UTF-16 编码）
                    let name_start = offset + file_name_offset;
                    let name_end = name_start + file_name_length;
                    if name_end <= bytes_returned as usize && file_name_length.is_multiple_of(2) {
                        let name_units: Vec<u16> = buffer[name_start..name_end]
                            .chunks_exact(2)
                            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                            .collect();
                        let file_name = OsString::from_wide(&name_units);

                        records.push(RawUsnRecord {
                            usn,
                            fid,
                            parent_fid,
                            reason,
                            file_name,
                            file_attributes,
                        });
                    }

                    offset += record_length as usize;
                }

                // 更新 current_start_usn 为 API 返回的 next_start_usn
                current_start_usn = next_start_usn;

                // 如果返回的数据量远小于缓冲区，说明已读完当前所有记录
                if (bytes_returned as usize) < buffer_size / 2 {
                    break;
                }
            }
            Err(err) => {
                // ERROR_HANDLE_EOF (38) 表示无更多数据，正常结束
                let code = err.code().0 & 0x0000FFFF;
                if code == 38 {
                    break;
                }
                // 其他错误返回错误信息
                return Err(format!(
                    "FSCTL_READ_USN_JOURNAL failed: {} (code=0x{:x})",
                    err, code
                ));
            }
        }
    }

    Ok((records, current_start_usn))
}

/// Fallback path resolution using batch parent map.
/// When usn-journal-rs's resolver can't find a parent (e.g. newly created files
/// whose parent wasn't in the resolver's MFT cache), we walk the parent chain
/// using the USN records from the current batch.
fn resolve_path_from_batch(
    fid: u64,
    parent_map: &std::collections::HashMap<u64, u64>,
    name_map: &std::collections::HashMap<u64, std::ffi::OsString>,
    drive_letter: char,
) -> Option<std::path::PathBuf> {
    let mut components: Vec<&std::ffi::OsString> = Vec::with_capacity(8);
    let mut cur = fid;

    // Walk up the parent chain (max 50 levels to prevent infinite loops)
    for _ in 0..50 {
        if let Some(name) = name_map.get(&cur) {
            let name_str = name.to_string_lossy();
            // Skip NTFS virtual entries
            if name_str != "." && name_str != ".." && !name_str.starts_with('$') {
                components.push(name);
            }
        }

        match parent_map.get(&cur) {
            Some(&pfid) if pfid != cur && pfid != 0 => {
                cur = pfid;
            }
            _ => break,
        }
    }

    if components.is_empty() {
        return None;
    }

    components.reverse();

    let mut path = std::path::PathBuf::new();
    path.push(format!("{}:", drive_letter));
    path.push("\\");
    for comp in &components {
        path.push(comp);
    }

    Some(path)
}

/// 从 dir_map 解析目录的完整路径
///
/// dir_map: record_number → (parent_record, dir_name)
/// 从 target_record 开始向上遍历 parent 链，拼接所有目录名。
///
/// 返回值：如 "C:\Windows\System32"（不含末尾反斜杠）
/// 如果 parent 链中有未知 record（不在 dir_map 中），返回 None
///
/// 注意：不跳过 $-前缀目录名，让它们出现在路径中。
/// $-前缀路径会在 callback 中被后续过滤（跳过路径中包含 $-前缀组件的条目）。
/// 这与旧代码（records_by_number 包含所有记录）的行为一致。
fn resolve_dir_path(
    target_record: u64,
    dir_map: &HashMap<u64, (u64, CompactString)>,
    drive_letter: char,
) -> Option<CompactString> {
    // record 0 无效
    if target_record == 0 {
        return None;
    }
    // record 5 是 NTFS 卷根目录，其 name 通常是 "."（被跳过，不会加入 components）
    // 直接返回 "X:"（仅盘符），调用方会拼接 "\" + name 得到 "X:\name"
    if target_record == 5 {
        return Some(CompactString::from(format!("{}:", drive_letter)));
    }

    let mut components: Vec<&CompactString> = Vec::with_capacity(16);
    let mut cur = target_record;
    for _ in 0..64 {
        // 防止循环引用
        match dir_map.get(&cur) {
            Some((parent, name)) => {
                // 跳过 NTFS 虚拟条目 (. 和 ..)
                // 注意：不跳过 $-前缀目录，让它们出现在路径中，后续统一过滤
                if !name.is_empty() && name != "." && name != ".." {
                    components.push(name);
                }
                // 到达根目录或循环（record 5 的 parent 通常为自身或 0）
                if *parent == cur || *parent == 0 {
                    break;
                }
                cur = *parent;
            }
            None => {
                // parent 未知，路径解析失败
                return None;
            }
        }
    }

    if components.is_empty() {
        return None;
    }

    components.reverse();

    // 拼接路径：C:\dir1\dir2
    let total_len: usize = components.iter().map(|c| c.len()).sum::<usize>()
        + components.len()  // 分隔符
        + 4; // "X:\" 前缀
    let mut path = String::with_capacity(total_len);
    path.push(drive_letter);
    path.push(':');
    path.push('\\');
    for (i, comp) in components.iter().enumerate() {
        if i > 0 {
            path.push('\\');
        }
        path.push_str(comp);
    }
    Some(CompactString::from(path))
}

pub fn spawn_usn_worker(cmd_rx: Receiver<UsnCommand>, resp_tx: Sender<UsnResponse>) {
    std::thread::Builder::new()
        .name("usn-worker".into())
        .spawn(move || {
            worker_loop(cmd_rx, resp_tx);
        })
        .expect("failed to spawn USN worker thread");
}

/// 将各卷 last_usn 持久化到 SQLite
fn save_usn_state(last_usn_map: &HashMap<char, i64>) {
    if last_usn_map.is_empty() {
        return;
    }
    let mut state = UsnState::default();
    for (&dl, &usn) in last_usn_map.iter() {
        state
            .volumes
            .insert(dl.to_string(), VolumeState { last_usn: usn });
    }
    state.save();
}

fn worker_loop(cmd_rx: Receiver<UsnCommand>, resp_tx: Sender<UsnResponse>) {
    let mut volumes: HashMap<char, Volume> = HashMap::new();
    let mut last_usn_map: HashMap<char, i64> = HashMap::new();
    let mut last_save_time = std::time::Instant::now();
    // last_usn 持久化间隔：1 小时。USN 进度丢失 1 小时内的变更通常可接受，
    // 下次启动时 journal 会从头读取或回退到全量扫描。
    let save_interval = std::time::Duration::from_secs(3600);
    // 复用 USN batch 路径解析缓冲区，避免每次轮询都重新分配 HashMap
    let mut batch_parent_buf: std::collections::HashMap<u64, u64> =
        std::collections::HashMap::new();
    let mut batch_name_buf: std::collections::HashMap<u64, std::ffi::OsString> =
        std::collections::HashMap::new();

    loop {
        match cmd_rx.recv() {
            Ok(UsnCommand::FullScan {
                drive_letter,
                include_hidden_files,
                include_system_files,
            }) => {
                handle_full_scan(
                    drive_letter,
                    include_hidden_files,
                    include_system_files,
                    &mut volumes,
                    &mut last_usn_map,
                    &resp_tx,
                );
            }
            Ok(UsnCommand::PollChanges {
                drive_letter,
                include_hidden_files,
                include_system_files,
            }) => {
                handle_poll_changes(
                    drive_letter,
                    include_hidden_files,
                    include_system_files,
                    &volumes,
                    &mut last_usn_map,
                    &mut last_save_time,
                    save_interval,
                    &resp_tx,
                    &mut batch_parent_buf,
                    &mut batch_name_buf,
                );
            }
            Ok(UsnCommand::Shutdown) | Err(_) => {
                // 关闭前立即持久化 last_usn，避免 1 小时间隔内崩溃/退出导致进度回退
                save_usn_state(&last_usn_map);
                log::info!("[USN] Worker shutdown, last_usn saved");
                break;
            }
        }
    }
}

fn handle_full_scan(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volumes: &mut HashMap<char, Volume>,
    last_usn_map: &mut HashMap<char, i64>,
    resp_tx: &Sender<UsnResponse>,
) {
    log::info!("[USN] Full scan starting for drive {}", drive_letter);
    let scan_start = Instant::now();

    // 流式扫描 + 目录优先策略
    //
    // 内存优势（对比旧方案）：
    // - 旧方案：all_records(260MB) + records_by_number(借用) + entries(180MB) = 440MB 峰值
    // - 新方案：dir_map(18MB) + files(106MB) + path_table(40MB) + deferred(<1MB) = 164MB 峰值
    // - 节省约 276MB
    //
    // 策略：
    // 1. 流式读取 MFT 记录，不收集到 Vec（消除 all_records 260MB）
    // 2. 目录立即加入 dir_map（仅 30万目录 × ~60字节 = 18MB）
    // 3. 文件/目录立即通过 dir_map 解析路径，构建 FileEntry（消除 entries 180MB）
    // 4. parent 未知的条目暂存到 deferred，扫描结束后统一处理
    let t0 = Instant::now();
    let volume_path = format!("\\\\.\\{}:", drive_letter);

    /// deferred 条目：record_number, parent_record, name, is_dir, mtime, size
    /// 用于暂存 parent 在扫描时还未加入 dir_map 的条目（通常 <1%）
    struct DeferredEntry {
        record_number: u64,
        parent_record: u64,
        name: CompactString,
        is_dir: bool,
        mtime: i64,
        size: u64,
    }

    let mut dir_map: HashMap<u64, (u64, CompactString)> = HashMap::with_capacity(500_000);
    let mut files: Vec<FileEntry> = Vec::with_capacity(2_000_000);
    let mut path_table = PathTable::new();
    let mut deferred: Vec<DeferredEntry> = Vec::new();
    // 暂存 pending_sizes（$ATTRIBUTE_LIST 扩展记录的真实 size）
    // 在 Phase 2 之后统一更新，确保 deferred 条目也被覆盖
    // Err 分支会提前 return，所以只有 Ok 分支会赋值，无需初始化为 None
    let pending_sizes_opt: Option<std::collections::HashMap<u64, u64>>;

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    // 路径过滤检查的闭包，避免重复代码
    // 返回 true 表示应跳过该路径
    let should_skip_path = |path_str: &str| -> bool {
        path_str.split('\\').any(|comp| {
            let sl = comp.to_lowercase();
            sl == RECYCLE_BIN || (!include_system_files && sl == SYSTEM_VOL_INFO)
        })
    };

    let stats = match mft_lib::scan_volume_streaming(&volume_path, u64::MAX, &mut |r| {
        // 跳过系统记录 (MFT 前 5 条: $MFT, $MFTMirr, $LogFile, $Volume, $AttrDef)
        if r.record_number < 5 {
            return;
        }
        // 跳过无名/占位记录 (<Record#N>, <no name>)
        if r.name.starts_with('<') {
            return;
        }

        let record_number = r.record_number;
        let parent_record = r.parent_record;
        let name = CompactString::from(r.name.as_str());
        let is_dir = r.is_directory;
        let mtime = ntfs_time_to_unix(r.mtime);
        let size = r.size;

        // 目录：必须在所有过滤之前加入 dir_map！
        // 原因：即使目录被 hidden/system/$-前缀过滤，它的子文件路径解析仍然需要
        // 通过 dir_map 查找 parent 链。如果被过滤的目录不在 dir_map 中，
        // 会导致 parent 链中断，所有子文件路径解析失败，被错误跳过。
        // 这就是之前少了100万文件的根本原因。
        if is_dir {
            dir_map.insert(record_number, (parent_record, name.clone()));
        }

        // 以下过滤仅影响是否构建 FileEntry，不影响 dir_map
        // 跳过 $-前缀的 NTFS 元数据文件 ($Bitmap, $Boot 等)
        if r.name.starts_with('$') {
            return;
        }
        // 跳过 NTFS 虚拟条目 (. 和 ..)
        if r.name == "." || r.name == ".." {
            return;
        }

        // hidden/system 属性检查
        if !include_hidden_files && (r.attributes & 0x02) != 0 {
            return;
        }
        if !include_system_files && (r.attributes & 0x04) != 0 {
            return;
        }

        // 解析完整路径
        // 通过 parent_record 在 dir_map 中查找父目录链
        // 分离 parent_path 和完整路径：
        // - parent_path_id 由 intern(parent_path) 得到，用于 FileEntry.path_id
        // - 完整路径仅用于过滤检查和目录自身注册
        // 这样 PathTable 只存储目录路径（~40万），避免为 221万文件存储完整路径
        let parent_path_id: u32;
        // 完整路径，用于过滤检查和目录自身注册
        let path_str: CompactString = if parent_record == 0 || parent_record == record_number {
            // parent 为 0 或自身：根目录级别的条目
            // 先 intern 根目录路径 "X:\" 得到 parent_path_id
            let mut root = String::with_capacity(3);
            root.push(drive_letter);
            root.push(':');
            root.push('\\');
            parent_path_id = path_table.intern(&root);
            // 构建 "X:\name" 形式用于过滤检查
            let mut path = String::with_capacity(root.len() + name.len());
            path.push_str(&root);
            path.push_str(&name);
            CompactString::from(path)
        } else {
            // 解析父目录路径
            match resolve_dir_path(parent_record, &dir_map, drive_letter) {
                Some(parent_path) => {
                    // intern 父目录路径得到 parent_path_id（FileEntry 使用）
                    parent_path_id = path_table.intern(&parent_path);
                    // 构建完整路径用于过滤检查
                    let mut path = String::with_capacity(parent_path.len() + 1 + name.len());
                    path.push_str(&parent_path);
                    path.push('\\');
                    path.push_str(&name);
                    CompactString::from(path)
                }
                None => {
                    // parent 未知，暂存到 deferred
                    // 扫描结束后 dir_map 完整，再统一处理
                    deferred.push(DeferredEntry {
                        record_number,
                        parent_record,
                        name,
                        is_dir,
                        mtime,
                        size,
                    });
                    return;
                }
            }
        };

        // 路径过滤：跳过 $recycle.bin 和 system volume information
        if should_skip_path(&path_str) {
            return;
        }

        // 跳过路径中包含 $-前缀组件的条目（NTFS 特殊目录）
        if path_str
            .split('\\')
            .any(|comp| comp.starts_with('$') && comp.len() > 1)
        {
            return;
        }

        // 对于目录，注册自身路径到 PathTable（供子条目 resolve_dir_path 使用）
        // 文件不需要注册，因为 FileEntry.path_id 指向父目录
        if is_dir {
            path_table.intern(&path_str);
        }

        // 构建 FileEntry 并推入 files
        // path_id 指向父目录，resolve_file_path(path_id, name) 可还原完整路径
        files.push(FileEntry::new(
            name,
            parent_path_id,
            size,
            mtime,
            record_number as u32,
            is_dir,
        ));
    }) {
        Ok((s, pending_sizes)) => {
            // 暂存 pending_sizes，待 Phase 2（deferred 处理）结束后再更新
            // 原因：deferred 条目在 Phase 2 才加入 files，此时更新会遗漏它们
            pending_sizes_opt = Some(pending_sizes);
            s
        }
        Err(e) => {
            log::warn!(
                "[USN] MftScanner streaming failed for {}: {}, falling back to FSCTL_ENUM_USN_DATA",
                drive_letter,
                e
            );
            handle_full_scan_legacy(
                drive_letter,
                include_hidden_files,
                include_system_files,
                volumes,
                last_usn_map,
                resp_tx,
            );
            return;
        }
    };

    log::info!(
        "[USN] Phase 1: Streamed {} records ({} files, {} dirs) for {} in {:?}, deferred={}",
        stats.total_records,
        stats.files,
        stats.dirs,
        drive_letter,
        t0.elapsed(),
        deferred.len()
    );

    // Phase 2: 处理 deferred 条目（此时 dir_map 已包含所有目录）
    let t1 = Instant::now();
    let deferred_count = deferred.len();
    for entry in deferred.drain(..) {
        // 分离 parent_path 和完整路径（与 Phase 1 callback 相同的逻辑）
        let parent_path_id: u32;
        let path_str: CompactString = if entry.parent_record == 0
            || entry.parent_record == entry.record_number
        {
            let mut root = String::with_capacity(3);
            root.push(drive_letter);
            root.push(':');
            root.push('\\');
            parent_path_id = path_table.intern(&root);
            let mut path = String::with_capacity(root.len() + entry.name.len());
            path.push_str(&root);
            path.push_str(&entry.name);
            CompactString::from(path)
        } else {
            match resolve_dir_path(entry.parent_record, &dir_map, drive_letter) {
                Some(parent_path) => {
                    parent_path_id = path_table.intern(&parent_path);
                    let mut path = String::with_capacity(parent_path.len() + 1 + entry.name.len());
                    path.push_str(&parent_path);
                    path.push('\\');
                    path.push_str(&entry.name);
                    CompactString::from(path)
                }
                None => {
                    // parent 仍然未知（可能是孤儿条目），跳过
                    continue;
                }
            }
        };

        if should_skip_path(&path_str) {
            continue;
        }
        if path_str
            .split('\\')
            .any(|comp| comp.starts_with('$') && comp.len() > 1)
        {
            continue;
        }

        // 目录注册自身路径，文件不需要
        if entry.is_dir {
            path_table.intern(&path_str);
        }

        // FileEntry.path_id 指向父目录
        files.push(FileEntry::new(
            entry.name,
            parent_path_id,
            entry.size,
            entry.mtime,
            entry.record_number as u32,
            entry.is_dir,
        ));
    }

    log::info!(
        "[USN] Phase 2: Resolved {} deferred entries in {:?}, total {} files (path_table size={})",
        deferred_count,
        t1.elapsed(),
        files.len(),
        path_table.len()
    );

    // Phase 3: 用 pending_sizes 更新 files 中 size=0 的 FileEntry
    // 必须在 Phase 2 之后执行，因为 deferred 条目在 Phase 2 才加入 files
    // pending_sizes: record_number → real_size（来自 $ATTRIBUTE_LIST 扩展记录）
    if let Some(ref pending_sizes) = pending_sizes_opt {
        if !pending_sizes.is_empty() {
            let mut updated_count = 0usize;
            let mut zero_size_count = 0usize;
            for f in files.iter_mut() {
                if f.size == 0 {
                    zero_size_count += 1;
                    // file_id 就是 MFT record_number
                    if let Some(&real_size) = pending_sizes.get(&(f.file_id as u64)) {
                        f.size = real_size;
                        updated_count += 1;
                    }
                }
            }
            log::info!(
                "[USN] Phase 3: Updated {} file sizes via pending_ext ({} resolved, {} files had size=0)",
                updated_count, pending_sizes.len(), zero_size_count
            );
        }
    }

    // 释放 dir_map 和 deferred（不再需要）
    drop(dir_map);
    drop(deferred);

    log::info!(
        "[USN] Full scan complete for drive {}: {} files, total {:?}",
        drive_letter,
        files.len(),
        scan_start.elapsed()
    );

    // 创建或验证 USN journal（增量更新所需）
    let volume = match Volume::from_drive_letter(drive_letter) {
        Ok(v) => v,
        Err(e) => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("Failed to open volume {} for journal: {}", drive_letter, e),
            });
            return;
        }
    };

    let journal = volume.journal();
    let journal_max_size = usn_journal_rs::DEFAULT_JOURNAL_MAX_SIZE;
    let allocation_delta = usn_journal_rs::DEFAULT_JOURNAL_ALLOCATION_DELTA;

    let (_journal_id, last_usn) = match journal.query(true) {
        Ok(data) => {
            if data.maximum_size < journal_max_size {
                if let Err(e) = journal.create_or_update(journal_max_size, allocation_delta) {
                    log::warn!("[USN] Failed to resize journal for {}: {}", drive_letter, e);
                }
                if let Ok(new_data) = journal.query(false) {
                    (new_data.journal_id, new_data.next_usn)
                } else {
                    (data.journal_id, data.next_usn)
                }
            } else {
                (data.journal_id, data.next_usn)
            }
        }
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            match journal.create_or_update(journal_max_size, allocation_delta) {
                Ok(()) => match journal.query(false) {
                    Ok(data) => (data.journal_id, data.next_usn),
                    Err(e2) => {
                        let _ = resp_tx.send(UsnResponse::Error {
                            drive_letter,
                            message: format!(
                                "Journal create+query failed for {}: {}",
                                drive_letter, e2
                            ),
                        });
                        return;
                    }
                },
                Err(e2) => {
                    let _ = resp_tx.send(UsnResponse::Error {
                        drive_letter,
                        message: format!("Journal create failed for {}: {}", drive_letter, e2),
                    });
                    return;
                }
            }
        }
    };

    last_usn_map.insert(drive_letter, last_usn);

    let mut state = UsnState::load();
    state
        .volumes
        .insert(drive_letter.to_string(), VolumeState { last_usn });
    state.save();

    volumes.insert(drive_letter, volume);

    let _ = resp_tx.send(UsnResponse::FullScanResult {
        drive_letter,
        files,
        path_table,
        last_usn,
    });
}

fn handle_full_scan_legacy(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volumes: &mut HashMap<char, Volume>,
    last_usn_map: &mut HashMap<char, i64>,
    resp_tx: &Sender<UsnResponse>,
) {
    log::info!(
        "[USN] Full scan (legacy) starting for drive {}",
        drive_letter
    );
    let scan_start = Instant::now();

    let volume = match Volume::from_drive_letter(drive_letter) {
        Ok(v) => v,
        Err(e) => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("Failed to open volume {}: {}", drive_letter, e),
            });
            return;
        }
    };

    let mft = volume.mft();

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    // Phase 1a: 自定义 MFT 枚举，直接从 USN_RECORD_V2 提取 timestamp
    // 4MB 缓冲区：减少 DeviceIoControl 调用次数至 2-3 次
    // 关键收益：timestamp 字段一次性拿到，省去后续 GetFileAttributesExW 查询
    let t0 = Instant::now();
    struct RawEntry {
        fid: u64,
        file_name: std::ffi::OsString,
        file_attributes: u32,
        timestamp: i64,
    }

    let mut raw_entries: Vec<RawEntry>;
    let mut parent_map: HashMap<u64, u64>;

    // 优先尝试自定义枚举器（携带 timestamp）
    // 失败时回退到 usn-journal-rs 的标准枚举器（无 timestamp）
    // 缓冲区选择：1MB 在 221 万文件下最优（实测 9.5s），
    // 4MB 反而因 buffer_size/2 退出条件过严导致多轮调用
    let vol_handle_for_mft = ntfs_mft::open_volume_handle(drive_letter);
    match vol_handle_for_mft {
        Some(h) => {
            match ntfs_mft::enumerate_mft_with_timestamps(h, 1024 * 1024) {
                Ok(entries_with_ts) => {
                    raw_entries = Vec::with_capacity(entries_with_ts.len());
                    parent_map = HashMap::with_capacity(entries_with_ts.len());
                    for e in entries_with_ts {
                        parent_map.insert(e.fid, e.parent_fid);
                        raw_entries.push(RawEntry {
                            fid: e.fid,
                            file_name: e.file_name,
                            file_attributes: e.file_attributes,
                            timestamp: e.timestamp,
                        });
                    }
                }
                Err(e) => {
                    log::warn!(
                        "[USN] Custom MFT enum failed ({}), falling back to usn-journal-rs",
                        e
                    );
                    let enum_options = EnumOptions {
                        buffer_size: 1024 * 1024,
                        ..Default::default()
                    };
                    raw_entries = Vec::with_capacity(1_000_000);
                    parent_map = HashMap::with_capacity(1_000_000);
                    for result in mft.iter_with_options(enum_options) {
                        let entry = match result {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        parent_map.insert(entry.fid, entry.parent_fid);
                        raw_entries.push(RawEntry {
                            fid: entry.fid,
                            file_name: entry.file_name,
                            file_attributes: entry.file_attributes,
                            timestamp: 0, // 回退路径无 timestamp
                        });
                    }
                }
            }
        }
        None => {
            log::warn!("[USN] Failed to open volume handle, falling back to usn-journal-rs");
            let enum_options = EnumOptions {
                buffer_size: 1024 * 1024,
                ..Default::default()
            };
            raw_entries = Vec::with_capacity(1_000_000);
            parent_map = HashMap::with_capacity(1_000_000);
            for result in mft.iter_with_options(enum_options) {
                let entry = match result {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                parent_map.insert(entry.fid, entry.parent_fid);
                raw_entries.push(RawEntry {
                    fid: entry.fid,
                    file_name: entry.file_name,
                    file_attributes: entry.file_attributes,
                    timestamp: 0,
                });
            }
        }
    }

    log::info!(
        "[USN] Phase 1a: Enumerated {} MFT entries for {} in {:?}",
        raw_entries.len(),
        drive_letter,
        t0.elapsed()
    );

    // Build O(1) index lookup: fid → raw_entries index (avoids cloning OsStrings)
    let fid_to_idx: HashMap<u64, usize> = raw_entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.fid, i))
        .collect();

    // Phase 1b: 并行路径解析，透传 MFT 阶段获取的 timestamp
    // entries 元组结构：(fid, name, path, is_dir, timestamp)
    let t1 = Instant::now();
    let entries: Vec<(u64, CompactString, CompactString, bool, i64)> = raw_entries
        .par_iter()
        .enumerate()
        .filter_map(|(_idx, re)| {
            // Skip $-prefixed NTFS metadata files (cheap pre-check before path resolution)
            if re.file_name.to_string_lossy().starts_with('$') {
                return None;
            }
            // Hidden check
            if !include_hidden_files && (re.file_attributes & 0x02) != 0 {
                return None;
            }
            // System check
            if !include_system_files && (re.file_attributes & 0x04) != 0 {
                return None;
            }

            // Resolve path via parent map — O(depth) with O(1) index lookups
            // Use Vec<&OsString> to borrow from raw_entries, avoiding clones
            let mut parts: Vec<&std::ffi::OsString> = Vec::with_capacity(16);
            parts.push(&re.file_name);
            let mut cur_fid = re.fid;
            for _ in 0..50 {
                match parent_map.get(&cur_fid) {
                    Some(&pfid) if pfid != cur_fid && pfid != 0 => {
                        cur_fid = pfid;
                        match fid_to_idx.get(&pfid) {
                            Some(&parent_idx) => {
                                let parent_name = &raw_entries[parent_idx].file_name;
                                // Skip NTFS virtual entries (. and ..) to avoid paths like C:\.\windows
                                if parent_name.to_string_lossy() == "."
                                    || parent_name.to_string_lossy() == ".."
                                {
                                    continue;
                                }
                                parts.push(parent_name);
                            }
                            None => break,
                        }
                    }
                    _ => break,
                }
            }
            parts.reverse();

            // Build path string with pre-allocated capacity
            let estimated_len: usize = parts.iter().map(|p| p.len()).sum::<usize>()
                + parts.len()  // separators
                + 4; // "X:\" prefix
            let path_str: CompactString = {
                let mut path = String::with_capacity(estimated_len);
                path.push(drive_letter);
                path.push(':');
                path.push('\\');
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        path.push('\\');
                    }
                    path.push_str(&part.to_string_lossy());
                }
                CompactString::from(path)
            };

            // Path component checks
            let skip = path_str.split('\\').any(|comp| {
                let sl = comp.to_lowercase();
                sl == RECYCLE_BIN || (!include_system_files && sl == SYSTEM_VOL_INFO)
            });
            if skip {
                return None;
            }

            // Pre-convert name to CompactString here so Phase 2 doesn't need name lookup
            let name_str: CompactString =
                CompactString::from(re.file_name.to_string_lossy().as_ref());
            let is_dir = (re.file_attributes & 0x10) != 0;
            Some((re.fid, name_str, path_str, is_dir, re.timestamp))
        })
        .collect();

    log::info!(
        "[USN] Phase 1b: Resolved {} file paths for {} in {:?}",
        entries.len(),
        drive_letter,
        t1.elapsed()
    );

    // Free raw_entries and parent_map/fid_to_idx — no longer needed
    drop(raw_entries);
    drop(parent_map);
    drop(fid_to_idx);

    // Phase 2: 仅查询文件大小（timestamp 已在 Phase 1a 从 MFT 拿到）
    // 优化收益：每个文件节省一次 timestamp 字段的传输，
    // 实际节省效果取决于 NTFS 元数据缓存命中率
    let t2 = Instant::now();
    let path_isdir: Vec<(CompactString, bool)> = entries
        .iter()
        .map(|(_, _, path, is_dir, _)| (path.clone(), *is_dir))
        .collect();
    let sizes = ntfs_mft::batch_metadata(&path_isdir);
    drop(path_isdir); // 释放临时内存

    // 并行查询 size 和 modified_time，准备 FileEntry 所需数据
    // PathTable::intern 需要可变借用，无法在 rayon 并行闭包中使用，
    // 因此先并行收集 (name, path, fid, is_dir, size, modified_time)，
    // 再在顺序循环中完成 path 注册和 FileEntry 构建
    let resolved: Vec<(CompactString, CompactString, u64, bool, u64, i64)> = entries
        .par_iter()
        .enumerate()
        .map(|(i, (fid, name_str, path_str, is_dir, timestamp))| {
            let (size, fallback_modified) = sizes[i];
            // 优先使用 MFT 时间戳，回退到 GetFileAttributesExW 返回的 modified_time
            let modified_time = if *timestamp > 0 {
                *timestamp
            } else {
                fallback_modified
            };
            (
                name_str.clone(),
                path_str.clone(),
                *fid,
                *is_dir,
                size,
                modified_time,
            )
        })
        .collect();

    // 顺序构建 FileEntry + PathTable（path_table.intern 需要顺序执行以保证 path_id 一致性）
    // 优化：FileEntry.path_id 指向父目录，而非完整文件路径
    // 这样 PathTable 只存储目录路径（~40万），避免为 221万文件存储完整路径字符串
    let mut path_table = PathTable::new();
    let files: Vec<FileEntry> = resolved
        .into_iter()
        .map(|(name_str, path_str, fid, is_dir, size, modified_time)| {
            // 从完整路径分离出父目录路径
            // path_str 格式："X:\dir\...\filename" 或 "X:\name"
            let parent_path_id = if let Some(pos) = path_str.rfind('\\') {
                if pos <= 2 {
                    // "X:\file" → parent = "X:\"
                    path_table.intern(&path_str[..pos + 1])
                } else {
                    // "X:\dir\file" → parent = "X:\dir"
                    path_table.intern(&path_str[..pos])
                }
            } else {
                // 异常情况：没有反斜杠，用根目录占位
                path_table.intern("X:\\")
            };

            // 对于目录，注册自身路径供子条目使用
            if is_dir {
                path_table.intern(&path_str);
            }

            // FileEntry::new 内部会将 modified_time (i64) 截断为 i32
            FileEntry::new(
                name_str,
                parent_path_id,
                size,
                modified_time,
                fid as u32,
                is_dir,
            )
        })
        .collect();

    log::info!(
        "[USN] Phase 2: Batch size lookup + FileEntry build for {} files (path_table size={}) in {:?}",
        files.len(), path_table.len(), t2.elapsed()
    );
    log::info!(
        "[USN] Full scan complete for drive {}: {} files, total {:?}",
        drive_letter,
        files.len(),
        scan_start.elapsed()
    );

    // Create or verify USN journal
    let journal = volume.journal();
    let journal_max_size = usn_journal_rs::DEFAULT_JOURNAL_MAX_SIZE;
    let allocation_delta = usn_journal_rs::DEFAULT_JOURNAL_ALLOCATION_DELTA;

    let (_journal_id, last_usn) = match journal.query(true) {
        Ok(data) => {
            if data.maximum_size < journal_max_size {
                if let Err(e) = journal.create_or_update(journal_max_size, allocation_delta) {
                    log::warn!("[USN] Failed to resize journal for {}: {}", drive_letter, e);
                }
                if let Ok(new_data) = journal.query(false) {
                    (new_data.journal_id, new_data.next_usn)
                } else {
                    (data.journal_id, data.next_usn)
                }
            } else {
                (data.journal_id, data.next_usn)
            }
        }
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            match journal.create_or_update(journal_max_size, allocation_delta) {
                Ok(()) => match journal.query(false) {
                    Ok(data) => (data.journal_id, data.next_usn),
                    Err(e2) => {
                        let _ = resp_tx.send(UsnResponse::Error {
                            drive_letter,
                            message: format!(
                                "Journal create+query failed for {}: {}",
                                drive_letter, e2
                            ),
                        });
                        return;
                    }
                },
                Err(e2) => {
                    let _ = resp_tx.send(UsnResponse::Error {
                        drive_letter,
                        message: format!("Journal create failed for {}: {}", drive_letter, e2),
                    });
                    return;
                }
            }
        }
    };

    last_usn_map.insert(drive_letter, last_usn);

    let mut state = UsnState::load();
    state
        .volumes
        .insert(drive_letter.to_string(), VolumeState { last_usn });
    state.save();

    volumes.insert(drive_letter, volume);

    let _ = resp_tx.send(UsnResponse::FullScanResult {
        drive_letter,
        files,
        path_table,
        last_usn,
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_poll_changes(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volumes: &HashMap<char, Volume>,
    last_usn_map: &mut HashMap<char, i64>,
    last_save_time: &mut std::time::Instant,
    save_interval: std::time::Duration,
    resp_tx: &Sender<UsnResponse>,
    batch_parent_buf: &mut std::collections::HashMap<u64, u64>,
    batch_name_buf: &mut std::collections::HashMap<u64, std::ffi::OsString>,
) {
    log::debug!(
        "[USN] handle_poll_changes entered for drive {}",
        drive_letter
    );
    let volume = match volumes.get(&drive_letter) {
        Some(v) => v,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("No volume handle for drive {}", drive_letter),
            });
            return;
        }
    };

    let last_usn = match last_usn_map.get(&drive_letter) {
        Some(&usn) => usn,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("No last USN recorded for drive {}", drive_letter),
            });
            return;
        }
    };

    let journal = volume.journal();

    // 查询当前 journal 状态（仅查询，不创建）
    let journal_data = match journal.query(false) {
        Ok(data) => data,
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("Failed to query journal for {}: {}", drive_letter, e),
            });
            return;
        }
    };

    // journal_id 一致性检查已移除：journal_id 不再持久化，
    // 如果日志被重建导致 last_usn 无效，轮询会报错并触发重新全量扫描。
    let effective_last_usn = last_usn;

    // 直接使用 effective_last_usn 作为 start_usn（不 +1）
    //
    // 关键：FSCTL_READ_USN_JOURNAL 的 StartUsn 必须是有效的记录边界
    // - 全量扫描后 last_usn = next_usn（有效的读取起点）
    // - 后续轮询 last_usn = API 返回的 next-start USN（也是有效的记录边界）
    // - last_usn + 1 不是有效记录边界，会导致 ERROR_INVALID_PARAMETER (0x80070057)
    //
    // 钳制到 [lowest_valid_usn, next_usn] 范围内以确保安全
    let start_usn = effective_last_usn
        .max(journal_data.lowest_valid_usn)
        .min(journal_data.next_usn);

    // 无新变更时直接返回；使用 debug 级别避免每次空轮询都产生 I/O
    if start_usn >= journal_data.next_usn {
        log::debug!(
            "[USN] Poll {}: no new records (start_usn={} >= next_usn={})",
            drive_letter,
            start_usn,
            journal_data.next_usn
        );
        return;
    }

    log::debug!(
        "[USN] Poll {}: start_usn={}, next_usn={}, using start={}",
        drive_letter,
        effective_last_usn,
        journal_data.next_usn,
        start_usn
    );

    // 直接调用 FSCTL_READ_USN_JOURNAL 读取变更记录
    let vol_handle = match ntfs_mft::open_volume_handle(drive_letter) {
        Some(h) => h,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("Failed to open volume handle for drive {}", drive_letter),
            });
            return;
        }
    };

    let (records, next_start_usn) =
        match read_usn_records_direct(vol_handle, journal_data.journal_id, start_usn) {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "[USN] Poll {}: read_usn_records_direct error: {}",
                    drive_letter,
                    e
                );
                let _ = resp_tx.send(UsnResponse::Error {
                    drive_letter,
                    message: format!("Failed to read journal for {}: {}", drive_letter, e),
                });
                return;
            }
        };

    // new_last_usn 使用 API 返回的 next_start_usn，这是有效的记录边界
    let new_last_usn = next_start_usn;

    // 空轮询时无需创建 resolver/mft_reader/HashMap，直接更新 last_usn 后返回
    if records.is_empty() {
        if new_last_usn > effective_last_usn {
            last_usn_map.insert(drive_letter, new_last_usn);
            let now = std::time::Instant::now();
            if now.duration_since(*last_save_time) >= save_interval {
                save_usn_state(last_usn_map);
                *last_save_time = now;
            }
        }
        log::debug!(
            "[USN] Poll {}: empty record batch, skipped processing",
            drive_letter
        );
        return;
    }

    let mut resolver = volume.path_resolver_with_cache();
    let mut added: Vec<SearchResult> = Vec::new();
    let mut removed: Vec<(u64, String)> = Vec::new();
    let mut updated: Vec<(u64, SearchResult)> = Vec::new();

    const USN_REASON_FILE_CREATE: u32 = 0x100;
    const USN_REASON_FILE_DELETE: u32 = 0x200;
    const USN_REASON_RENAME_OLD_NAME: u32 = 0x1000;
    const USN_REASON_RENAME_NEW_NAME: u32 = 0x2000;
    const USN_REASON_DATA_OVERWRITE: u32 = 0x01;
    const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x8000;

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    // Build a parent map from this batch of USN records for fallback path resolution.
    // When usn-journal-rs's resolver can't find a parent (e.g. newly created files),
    // we can walk the parent chain using this map.
    // 复用 worker_loop 中的缓冲区，避免每次轮询都重新分配 HashMap
    batch_parent_buf.clear();
    batch_name_buf.clear();
    for rec in &records {
        batch_parent_buf.insert(rec.fid, rec.parent_fid);
        batch_name_buf.insert(rec.fid, rec.file_name.clone());
    }

    // 复用 volume handle 用于元数据查询
    let mut mft_reader =
        ntfs_mft::open_volume_handle(drive_letter).map(ntfs_mft::UsnMetadataReader::new);

    let entry_count = records.len();

    // Dedup: same fid can appear in multiple USN records (create + write + close).
    // Track which fids have already been added/removed/updated in this batch.
    let mut added_fids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut removed_fids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    for entry in &records {
        let reason = entry.reason;
        let fid = entry.fid;

        // Handle deletion/renamed-away: mark old fid as removed
        if reason & USN_REASON_FILE_DELETE != 0 || reason & USN_REASON_RENAME_OLD_NAME != 0 {
            if removed_fids.insert(fid) {
                // 解析被删除/重命名旧路径的完整路径，用于 base 中 fid 不匹配时兜底定位
                let del_path = match resolver.resolve_path(entry) {
                    Some(p) => p.to_string_lossy().to_string(),
                    None => {
                        match resolve_path_from_batch(
                            fid,
                            batch_parent_buf,
                            batch_name_buf,
                            drive_letter,
                        ) {
                            Some(p) => p.to_string_lossy().to_string(),
                            None => {
                                log::warn!(
                                    "[USN] resolve_path FAILED (removed) for fid={}, name={}, reason=0x{:x}",
                                    fid,
                                    entry.file_name.to_string_lossy(),
                                    reason
                                );
                                String::new()
                            }
                        }
                    }
                };
                removed.push((fid, del_path));
            }
            // Clear added_fids so that a subsequent RENAME_NEW_NAME for the same
            // fid (same file, new name) can re-add it. Without this, a new file
            // that is created+renamed in the same batch would be skipped by dedup.
            added_fids.remove(&fid);
        }

        // Handle creation/renamed-to: add new entry
        // NOTE: use independent `if` (not `else if`) so rename records with both
        // RENAME_OLD_NAME + RENAME_NEW_NAME flags process both branches.
        if reason & USN_REASON_FILE_CREATE != 0 || reason & USN_REASON_RENAME_NEW_NAME != 0 {
            // Skip if this fid was already added in this batch (same file can have
            // multiple USN records: create + write + close)
            if !added_fids.insert(fid) {
                continue;
            }
            let path = match resolver.resolve_path(entry) {
                Some(p) => p,
                None => {
                    // Fallback: build path from batch parent map
                    match resolve_path_from_batch(
                        fid,
                        batch_parent_buf,
                        batch_name_buf,
                        drive_letter,
                    ) {
                        Some(p) => p,
                        None => {
                            log::warn!(
                                "[USN] resolve_path FAILED for fid={}, name={}, reason=0x{:x}, is_dir={}",
                                fid,
                                entry.file_name.to_string_lossy(),
                                reason,
                                entry.is_dir()
                            );
                            continue;
                        }
                    }
                }
            };

            let skip = path.components().any(|comp| {
                let s = comp.as_os_str().to_string_lossy();
                let sl = s.to_lowercase();
                if sl == RECYCLE_BIN {
                    return true;
                }
                if !include_system_files && sl == SYSTEM_VOL_INFO {
                    return true;
                }
                false
            });
            if skip {
                continue;
            }

            if !include_hidden_files && entry.is_hidden() {
                continue;
            }
            if entry.file_name.to_string_lossy().starts_with('$') {
                continue;
            }

            let name: CompactString =
                CompactString::from(entry.file_name.to_string_lossy().as_ref());
            let path_str: CompactString = CompactString::from(path.to_string_lossy().as_ref());
            let meta = mft_reader
                .as_mut()
                .map(|r| r.get_file_metadata(fid, &path))
                .unwrap_or(ntfs_mft::FileMetadata {
                    size: 0,
                    modified_time: 0,
                    is_directory: entry.is_dir(),
                });
            added.push(SearchResult {
                file_id: fid as u32,
                name,
                path: path_str,
                size: meta.size,
                modified_time: meta.modified_time,
                is_directory: meta.is_directory,
            });
        }

        // Handle data/info change
        if reason & USN_REASON_DATA_OVERWRITE != 0 || reason & USN_REASON_BASIC_INFO_CHANGE != 0 {
            let path = match resolver.resolve_path(entry) {
                Some(p) => p,
                None => {
                    // Fallback: build path from batch parent map
                    match resolve_path_from_batch(
                        fid,
                        batch_parent_buf,
                        batch_name_buf,
                        drive_letter,
                    ) {
                        Some(p) => p,
                        None => {
                            log::warn!(
                                "[USN] resolve_path FAILED (update) for fid={}, name={}, reason=0x{:x}",
                                fid,
                                entry.file_name.to_string_lossy(),
                                reason
                            );
                            continue;
                        }
                    }
                }
            };

            let name: CompactString =
                CompactString::from(entry.file_name.to_string_lossy().as_ref());
            let path_str: CompactString = CompactString::from(path.to_string_lossy().as_ref());
            let meta = mft_reader
                .as_mut()
                .map(|r| r.get_file_metadata(fid, &path))
                .unwrap_or(ntfs_mft::FileMetadata {
                    size: 0,
                    modified_time: 0,
                    is_directory: entry.is_dir(),
                });
            updated.push((
                fid,
                SearchResult {
                    file_id: fid as u32,
                    name,
                    path: path_str,
                    size: meta.size,
                    modified_time: meta.modified_time,
                    is_directory: meta.is_directory,
                },
            ));
        }
    }

    log::info!(
        "[USN] Poll {}: entries_read={}, added={}, removed={}, updated={}, new_last_usn={}",
        drive_letter,
        entry_count,
        added.len(),
        removed.len(),
        updated.len(),
        new_last_usn
    );

    // 仅当 new_last_usn 有进展时更新持久化状态，且距上次保存超过 1 小时
    if new_last_usn > effective_last_usn {
        last_usn_map.insert(drive_letter, new_last_usn);

        let now = std::time::Instant::now();
        if now.duration_since(*last_save_time) >= save_interval {
            save_usn_state(last_usn_map);
            *last_save_time = now;
        }
    }

    // 仅当有实际变更时才发送结果，避免触发无意义的缓存更新和前端刷新
    if added.is_empty() && removed.is_empty() && updated.is_empty() {
        log::debug!(
            "[USN] Poll {}: no relevant changes after filtering, skipping notification",
            drive_letter
        );
        return;
    }

    let _ = resp_tx.send(UsnResponse::IncrementalResult {
        drive_letter,
        added,
        removed,
        updated,
        last_usn: new_last_usn,
    });
}

// ── Single-volume worker functions ──────────────────────────────────────────

pub fn spawn_usn_worker_for_volume(
    drive_letter: char,
    cmd_rx: Receiver<UsnCommand>,
    resp_tx: Sender<UsnResponse>,
) {
    std::thread::Builder::new()
        .name(format!("usn-worker-{}", drive_letter))
        .spawn(move || {
            let mut volume: Option<Volume> = None;
            let mut last_usn: i64 = 0;
            let mut last_save_time = Instant::now();
            let save_interval = std::time::Duration::from_secs(3600);
            let mut batch_parent_buf: HashMap<u64, u64> = HashMap::new();
            let mut batch_name_buf: HashMap<u64, OsString> = HashMap::new();

            worker_loop_single_volume(
                drive_letter,
                &mut volume,
                &mut last_usn,
                &mut last_save_time,
                save_interval,
                &mut batch_parent_buf,
                &mut batch_name_buf,
                cmd_rx,
                resp_tx,
            );
        })
        .expect("failed to spawn USN worker thread");
}

fn save_volume_state(drive_letter: char, last_usn: i64) {
    let mut state = UsnState::load();
    state
        .volumes
        .insert(drive_letter.to_string(), VolumeState { last_usn });
    state.save();
}

fn worker_loop_single_volume(
    drive_letter: char,
    volume: &mut Option<Volume>,
    last_usn: &mut i64,
    last_save_time: &mut Instant,
    save_interval: std::time::Duration,
    batch_parent_buf: &mut HashMap<u64, u64>,
    batch_name_buf: &mut HashMap<u64, OsString>,
    cmd_rx: Receiver<UsnCommand>,
    resp_tx: Sender<UsnResponse>,
) {
    loop {
        match cmd_rx.recv() {
            Ok(UsnCommand::FullScan {
                drive_letter: cmd_dl,
                include_hidden_files,
                include_system_files,
            }) => {
                handle_full_scan_for_volume(
                    cmd_dl,
                    include_hidden_files,
                    include_system_files,
                    volume,
                    last_usn,
                    &resp_tx,
                );
            }
            Ok(UsnCommand::PollChanges {
                drive_letter: cmd_dl,
                include_hidden_files,
                include_system_files,
            }) => {
                handle_poll_changes_for_volume(
                    cmd_dl,
                    include_hidden_files,
                    include_system_files,
                    volume,
                    last_usn,
                    last_save_time,
                    save_interval,
                    &resp_tx,
                    batch_parent_buf,
                    batch_name_buf,
                );
            }
            Ok(UsnCommand::Shutdown) | Err(_) => {
                if *last_usn != 0 {
                    save_volume_state(drive_letter, *last_usn);
                }
                log::info!("[USN] Worker-{} shutdown, last_usn saved", drive_letter);
                break;
            }
        }
    }
}

fn handle_full_scan_for_volume(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volume: &mut Option<Volume>,
    last_usn: &mut i64,
    resp_tx: &Sender<UsnResponse>,
) {
    log::info!(
        "[USN] Full scan (single-volume) starting for drive {}",
        drive_letter
    );
    let scan_start = Instant::now();

    let t0 = Instant::now();
    let volume_path = format!("\\\\.\\{}:", drive_letter);

    struct DeferredEntry {
        record_number: u64,
        parent_record: u64,
        name: CompactString,
        is_dir: bool,
        mtime: i64,
        size: u64,
    }

    let mut dir_map: HashMap<u64, (u64, CompactString)> = HashMap::with_capacity(500_000);
    let mut files: Vec<FileEntry> = Vec::with_capacity(2_000_000);
    let mut path_table = PathTable::new();
    let mut deferred: Vec<DeferredEntry> = Vec::new();
    let pending_sizes_opt: Option<std::collections::HashMap<u64, u64>>;

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    let should_skip_path = |path_str: &str| -> bool {
        path_str.split('\\').any(|comp| {
            let sl = comp.to_lowercase();
            sl == RECYCLE_BIN || (!include_system_files && sl == SYSTEM_VOL_INFO)
        })
    };

    let stats = match mft_lib::scan_volume_streaming(&volume_path, u64::MAX, &mut |r| {
        if r.record_number < 5 {
            return;
        }
        if r.name.starts_with('<') {
            return;
        }

        let record_number = r.record_number;
        let parent_record = r.parent_record;
        let name = CompactString::from(r.name.as_str());
        let is_dir = r.is_directory;
        let mtime = ntfs_time_to_unix(r.mtime);
        let size = r.size;

        if is_dir {
            dir_map.insert(record_number, (parent_record, name.clone()));
        }

        if r.name.starts_with('$') {
            return;
        }
        if r.name == "." || r.name == ".." {
            return;
        }

        if !include_hidden_files && (r.attributes & 0x02) != 0 {
            return;
        }
        if !include_system_files && (r.attributes & 0x04) != 0 {
            return;
        }

        let parent_path_id: u32;
        let path_str: CompactString = if parent_record == 0 || parent_record == record_number {
            let mut root = String::with_capacity(3);
            root.push(drive_letter);
            root.push(':');
            root.push('\\');
            parent_path_id = path_table.intern(&root);
            let mut path = String::with_capacity(root.len() + name.len());
            path.push_str(&root);
            path.push_str(&name);
            CompactString::from(path)
        } else {
            match resolve_dir_path(parent_record, &dir_map, drive_letter) {
                Some(parent_path) => {
                    parent_path_id = path_table.intern(&parent_path);
                    let mut path =
                        String::with_capacity(parent_path.len() + 1 + name.len());
                    path.push_str(&parent_path);
                    path.push('\\');
                    path.push_str(&name);
                    CompactString::from(path)
                }
                None => {
                    deferred.push(DeferredEntry {
                        record_number,
                        parent_record,
                        name,
                        is_dir,
                        mtime,
                        size,
                    });
                    return;
                }
            }
        };

        if should_skip_path(&path_str) {
            return;
        }

        if path_str
            .split('\\')
            .any(|comp| comp.starts_with('$') && comp.len() > 1)
        {
            return;
        }

        if is_dir {
            path_table.intern(&path_str);
        }

        files.push(FileEntry::new(
            name,
            parent_path_id,
            size,
            mtime,
            record_number as u32,
            is_dir,
        ));
    }) {
        Ok((s, pending_sizes)) => {
            pending_sizes_opt = Some(pending_sizes);
            s
        }
        Err(e) => {
            log::warn!(
                "[USN] MftScanner streaming failed for {}: {}, falling back to legacy",
                drive_letter,
                e
            );
            handle_full_scan_legacy_for_volume(
                drive_letter,
                include_hidden_files,
                include_system_files,
                volume,
                last_usn,
                resp_tx,
            );
            return;
        }
    };

    log::info!(
        "[USN] Phase 1: Streamed {} records ({} files, {} dirs) for {} in {:?}, deferred={}",
        stats.total_records,
        stats.files,
        stats.dirs,
        drive_letter,
        t0.elapsed(),
        deferred.len()
    );

    let t1 = Instant::now();
    let deferred_count = deferred.len();
    for entry in deferred.drain(..) {
        let parent_path_id: u32;
        let path_str: CompactString = if entry.parent_record == 0
            || entry.parent_record == entry.record_number
        {
            let mut root = String::with_capacity(3);
            root.push(drive_letter);
            root.push(':');
            root.push('\\');
            parent_path_id = path_table.intern(&root);
            let mut path = String::with_capacity(root.len() + entry.name.len());
            path.push_str(&root);
            path.push_str(&entry.name);
            CompactString::from(path)
        } else {
            match resolve_dir_path(entry.parent_record, &dir_map, drive_letter) {
                Some(parent_path) => {
                    parent_path_id = path_table.intern(&parent_path);
                    let mut path = String::with_capacity(
                        parent_path.len() + 1 + entry.name.len(),
                    );
                    path.push_str(&parent_path);
                    path.push('\\');
                    path.push_str(&entry.name);
                    CompactString::from(path)
                }
                None => {
                    continue;
                }
            }
        };

        if should_skip_path(&path_str) {
            continue;
        }
        if path_str
            .split('\\')
            .any(|comp| comp.starts_with('$') && comp.len() > 1)
        {
            continue;
        }

        if entry.is_dir {
            path_table.intern(&path_str);
        }

        files.push(FileEntry::new(
            entry.name,
            parent_path_id,
            entry.size,
            entry.mtime,
            entry.record_number as u32,
            entry.is_dir,
        ));
    }

    log::info!(
        "[USN] Phase 2: Resolved {} deferred entries in {:?}, total {} files (path_table size={})",
        deferred_count,
        t1.elapsed(),
        files.len(),
        path_table.len()
    );

    if let Some(ref pending_sizes) = pending_sizes_opt {
        if !pending_sizes.is_empty() {
            let mut updated_count = 0usize;
            let mut zero_size_count = 0usize;
            for f in files.iter_mut() {
                if f.size == 0 {
                    zero_size_count += 1;
                    if let Some(&real_size) = pending_sizes.get(&(f.file_id as u64)) {
                        f.size = real_size;
                        updated_count += 1;
                    }
                }
            }
            log::info!(
                "[USN] Phase 3: Updated {} file sizes via pending_ext ({} resolved, {} files had size=0)",
                updated_count,
                pending_sizes.len(),
                zero_size_count
            );
        }
    }

    drop(dir_map);
    drop(deferred);

    log::info!(
        "[USN] Full scan complete for drive {}: {} files, total {:?}",
        drive_letter,
        files.len(),
        scan_start.elapsed()
    );

    let new_volume = match Volume::from_drive_letter(drive_letter) {
        Ok(v) => v,
        Err(e) => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("Failed to open volume {} for journal: {}", drive_letter, e),
            });
            return;
        }
    };

    let journal = new_volume.journal();
    let journal_max_size = usn_journal_rs::DEFAULT_JOURNAL_MAX_SIZE;
    let allocation_delta = usn_journal_rs::DEFAULT_JOURNAL_ALLOCATION_DELTA;

    let (_journal_id, journal_last_usn) = match journal.query(true) {
        Ok(data) => {
            if data.maximum_size < journal_max_size {
                if let Err(e) = journal.create_or_update(journal_max_size, allocation_delta) {
                    log::warn!("[USN] Failed to resize journal for {}: {}", drive_letter, e);
                }
                if let Ok(new_data) = journal.query(false) {
                    (new_data.journal_id, new_data.next_usn)
                } else {
                    (data.journal_id, data.next_usn)
                }
            } else {
                (data.journal_id, data.next_usn)
            }
        }
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            match journal.create_or_update(journal_max_size, allocation_delta) {
                Ok(()) => match journal.query(false) {
                    Ok(data) => (data.journal_id, data.next_usn),
                    Err(e2) => {
                        let _ = resp_tx.send(UsnResponse::Error {
                            drive_letter,
                            message: format!(
                                "Journal create+query failed for {}: {}",
                                drive_letter, e2
                            ),
                        });
                        return;
                    }
                },
                Err(e2) => {
                    let _ = resp_tx.send(UsnResponse::Error {
                        drive_letter,
                        message: format!("Journal create failed for {}: {}", drive_letter, e2),
                    });
                    return;
                }
            }
        }
    };

    *last_usn = journal_last_usn;
    save_volume_state(drive_letter, journal_last_usn);

    *volume = Some(new_volume);

    let _ = resp_tx.send(UsnResponse::FullScanResult {
        drive_letter,
        files,
        path_table,
        last_usn: journal_last_usn,
    });
}

fn handle_full_scan_legacy_for_volume(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volume: &mut Option<Volume>,
    last_usn: &mut i64,
    resp_tx: &Sender<UsnResponse>,
) {
    log::info!(
        "[USN] Full scan (legacy, single-volume) starting for drive {}",
        drive_letter
    );
    let scan_start = Instant::now();

    let vol = match Volume::from_drive_letter(drive_letter) {
        Ok(v) => v,
        Err(e) => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("Failed to open volume {}: {}", drive_letter, e),
            });
            return;
        }
    };

    let mft = vol.mft();

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    let t0 = Instant::now();
    struct RawEntry {
        fid: u64,
        file_name: std::ffi::OsString,
        file_attributes: u32,
        timestamp: i64,
    }

    let mut raw_entries: Vec<RawEntry>;
    let mut parent_map: HashMap<u64, u64>;

    let vol_handle_for_mft = ntfs_mft::open_volume_handle(drive_letter);
    match vol_handle_for_mft {
        Some(h) => {
            match ntfs_mft::enumerate_mft_with_timestamps(h, 1024 * 1024) {
                Ok(entries_with_ts) => {
                    raw_entries = Vec::with_capacity(entries_with_ts.len());
                    parent_map = HashMap::with_capacity(entries_with_ts.len());
                    for e in entries_with_ts {
                        parent_map.insert(e.fid, e.parent_fid);
                        raw_entries.push(RawEntry {
                            fid: e.fid,
                            file_name: e.file_name,
                            file_attributes: e.file_attributes,
                            timestamp: e.timestamp,
                        });
                    }
                }
                Err(e) => {
                    log::warn!(
                        "[USN] Custom MFT enum failed ({}), falling back to usn-journal-rs",
                        e
                    );
                    let enum_options = EnumOptions {
                        buffer_size: 1024 * 1024,
                        ..Default::default()
                    };
                    raw_entries = Vec::with_capacity(1_000_000);
                    parent_map = HashMap::with_capacity(1_000_000);
                    for result in mft.iter_with_options(enum_options) {
                        let entry = match result {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        parent_map.insert(entry.fid, entry.parent_fid);
                        raw_entries.push(RawEntry {
                            fid: entry.fid,
                            file_name: entry.file_name,
                            file_attributes: entry.file_attributes,
                            timestamp: 0,
                        });
                    }
                }
            }
        }
        None => {
            log::warn!("[USN] Failed to open volume handle, falling back to usn-journal-rs");
            let enum_options = EnumOptions {
                buffer_size: 1024 * 1024,
                ..Default::default()
            };
            raw_entries = Vec::with_capacity(1_000_000);
            parent_map = HashMap::with_capacity(1_000_000);
            for result in mft.iter_with_options(enum_options) {
                let entry = match result {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                parent_map.insert(entry.fid, entry.parent_fid);
                raw_entries.push(RawEntry {
                    fid: entry.fid,
                    file_name: entry.file_name,
                    file_attributes: entry.file_attributes,
                    timestamp: 0,
                });
            }
        }
    }

    log::info!(
        "[USN] Phase 1a: Enumerated {} MFT entries for {} in {:?}",
        raw_entries.len(),
        drive_letter,
        t0.elapsed()
    );

    let fid_to_idx: HashMap<u64, usize> = raw_entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.fid, i))
        .collect();

    let t1 = Instant::now();
    let entries: Vec<(u64, CompactString, CompactString, bool, i64)> = raw_entries
        .par_iter()
        .enumerate()
        .filter_map(|(_idx, re)| {
            if re.file_name.to_string_lossy().starts_with('$') {
                return None;
            }
            if !include_hidden_files && (re.file_attributes & 0x02) != 0 {
                return None;
            }
            if !include_system_files && (re.file_attributes & 0x04) != 0 {
                return None;
            }

            let mut parts: Vec<&std::ffi::OsString> = Vec::with_capacity(16);
            parts.push(&re.file_name);
            let mut cur_fid = re.fid;
            for _ in 0..50 {
                match parent_map.get(&cur_fid) {
                    Some(&pfid) if pfid != cur_fid && pfid != 0 => {
                        cur_fid = pfid;
                        match fid_to_idx.get(&pfid) {
                            Some(&parent_idx) => {
                                let parent_name = &raw_entries[parent_idx].file_name;
                                if parent_name.to_string_lossy() == "."
                                    || parent_name.to_string_lossy() == ".."
                                {
                                    continue;
                                }
                                parts.push(parent_name);
                            }
                            None => break,
                        }
                    }
                    _ => break,
                }
            }
            parts.reverse();

            let estimated_len: usize = parts.iter().map(|p| p.len()).sum::<usize>()
                + parts.len()
                + 4;
            let path_str: CompactString = {
                let mut path = String::with_capacity(estimated_len);
                path.push(drive_letter);
                path.push(':');
                path.push('\\');
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        path.push('\\');
                    }
                    path.push_str(&part.to_string_lossy());
                }
                CompactString::from(path)
            };

            let skip = path_str.split('\\').any(|comp| {
                let sl = comp.to_lowercase();
                sl == RECYCLE_BIN || (!include_system_files && sl == SYSTEM_VOL_INFO)
            });
            if skip {
                return None;
            }

            let name_str: CompactString =
                CompactString::from(re.file_name.to_string_lossy().as_ref());
            let is_dir = (re.file_attributes & 0x10) != 0;
            Some((re.fid, name_str, path_str, is_dir, re.timestamp))
        })
        .collect();

    log::info!(
        "[USN] Phase 1b: Resolved {} file paths for {} in {:?}",
        entries.len(),
        drive_letter,
        t1.elapsed()
    );

    drop(raw_entries);
    drop(parent_map);
    drop(fid_to_idx);

    let t2 = Instant::now();
    let path_isdir: Vec<(CompactString, bool)> = entries
        .iter()
        .map(|(_, _, path, is_dir, _)| (path.clone(), *is_dir))
        .collect();
    let sizes = ntfs_mft::batch_metadata(&path_isdir);
    drop(path_isdir);

    let resolved: Vec<(CompactString, CompactString, u64, bool, u64, i64)> = entries
        .par_iter()
        .enumerate()
        .map(|(i, (fid, name_str, path_str, is_dir, timestamp))| {
            let (size, fallback_modified) = sizes[i];
            let modified_time = if *timestamp > 0 {
                *timestamp
            } else {
                fallback_modified
            };
            (
                name_str.clone(),
                path_str.clone(),
                *fid,
                *is_dir,
                size,
                modified_time,
            )
        })
        .collect();

    let mut path_table = PathTable::new();
    let files: Vec<FileEntry> = resolved
        .into_iter()
        .map(|(name_str, path_str, fid, is_dir, size, modified_time)| {
            let parent_path_id = if let Some(pos) = path_str.rfind('\\') {
                if pos <= 2 {
                    path_table.intern(&path_str[..pos + 1])
                } else {
                    path_table.intern(&path_str[..pos])
                }
            } else {
                path_table.intern("X:\\")
            };

            if is_dir {
                path_table.intern(&path_str);
            }

            FileEntry::new(
                name_str,
                parent_path_id,
                size,
                modified_time,
                fid as u32,
                is_dir,
            )
        })
        .collect();

    log::info!(
        "[USN] Phase 2: Batch size lookup + FileEntry build for {} files (path_table size={}) in {:?}",
        files.len(),
        path_table.len(),
        t2.elapsed()
    );
    log::info!(
        "[USN] Full scan (legacy) complete for drive {}: {} files, total {:?}",
        drive_letter,
        files.len(),
        scan_start.elapsed()
    );

    let journal = vol.journal();
    let journal_max_size = usn_journal_rs::DEFAULT_JOURNAL_MAX_SIZE;
    let allocation_delta = usn_journal_rs::DEFAULT_JOURNAL_ALLOCATION_DELTA;

    let (_journal_id, journal_last_usn) = match journal.query(true) {
        Ok(data) => {
            if data.maximum_size < journal_max_size {
                if let Err(e) = journal.create_or_update(journal_max_size, allocation_delta) {
                    log::warn!("[USN] Failed to resize journal for {}: {}", drive_letter, e);
                }
                if let Ok(new_data) = journal.query(false) {
                    (new_data.journal_id, new_data.next_usn)
                } else {
                    (data.journal_id, data.next_usn)
                }
            } else {
                (data.journal_id, data.next_usn)
            }
        }
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            match journal.create_or_update(journal_max_size, allocation_delta) {
                Ok(()) => match journal.query(false) {
                    Ok(data) => (data.journal_id, data.next_usn),
                    Err(e2) => {
                        let _ = resp_tx.send(UsnResponse::Error {
                            drive_letter,
                            message: format!(
                                "Journal create+query failed for {}: {}",
                                drive_letter, e2
                            ),
                        });
                        return;
                    }
                },
                Err(e2) => {
                    let _ = resp_tx.send(UsnResponse::Error {
                        drive_letter,
                        message: format!("Journal create failed for {}: {}", drive_letter, e2),
                    });
                    return;
                }
            }
        }
    };

    *last_usn = journal_last_usn;
    save_volume_state(drive_letter, journal_last_usn);

    *volume = Some(vol);

    let _ = resp_tx.send(UsnResponse::FullScanResult {
        drive_letter,
        files,
        path_table,
        last_usn: journal_last_usn,
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_poll_changes_for_volume(
    drive_letter: char,
    include_hidden_files: bool,
    include_system_files: bool,
    volume: &Option<Volume>,
    last_usn: &mut i64,
    last_save_time: &mut Instant,
    save_interval: std::time::Duration,
    resp_tx: &Sender<UsnResponse>,
    batch_parent_buf: &mut HashMap<u64, u64>,
    batch_name_buf: &mut HashMap<u64, OsString>,
) {
    log::debug!(
        "[USN] handle_poll_changes_for_volume entered for drive {}",
        drive_letter
    );
    let vol = match volume {
        Some(v) => v,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("No volume handle for drive {}", drive_letter),
            });
            return;
        }
    };

    let effective_last_usn = *last_usn;

    let journal = vol.journal();

    let journal_data = match journal.query(false) {
        Ok(data) => data,
        Err(e) => {
            log::warn!("[USN] Failed to query journal for {}: {}", drive_letter, e);
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("Failed to query journal for {}: {}", drive_letter, e),
            });
            return;
        }
    };

    let start_usn = effective_last_usn
        .max(journal_data.lowest_valid_usn)
        .min(journal_data.next_usn);

    if start_usn >= journal_data.next_usn {
        log::debug!(
            "[USN] Poll-{}: no new records (start_usn={} >= next_usn={})",
            drive_letter,
            start_usn,
            journal_data.next_usn
        );
        return;
    }

    log::debug!(
        "[USN] Poll-{}: start_usn={}, next_usn={}, using start={}",
        drive_letter,
        effective_last_usn,
        journal_data.next_usn,
        start_usn
    );

    let vol_handle = match ntfs_mft::open_volume_handle(drive_letter) {
        Some(h) => h,
        None => {
            let _ = resp_tx.send(UsnResponse::Error {
                drive_letter,
                message: format!("Failed to open volume handle for drive {}", drive_letter),
            });
            return;
        }
    };

    let (records, next_start_usn) =
        match read_usn_records_direct(vol_handle, journal_data.journal_id, start_usn) {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "[USN] Poll-{}: read_usn_records_direct error: {}",
                    drive_letter,
                    e
                );
                let _ = resp_tx.send(UsnResponse::Error {
                    drive_letter,
                    message: format!("Failed to read journal for {}: {}", drive_letter, e),
                });
                return;
            }
        };

    let new_last_usn = next_start_usn;

    if records.is_empty() {
        if new_last_usn > effective_last_usn {
            *last_usn = new_last_usn;
            let now = Instant::now();
            if now.duration_since(*last_save_time) >= save_interval {
                save_volume_state(drive_letter, new_last_usn);
                *last_save_time = now;
            }
        }
        log::debug!(
            "[USN] Poll-{}: empty record batch, skipped processing",
            drive_letter
        );
        return;
    }

    let mut resolver = vol.path_resolver_with_cache();

    let mut added: Vec<SearchResult> = Vec::new();
    let mut removed: Vec<(u64, String)> = Vec::new();
    let mut updated: Vec<(u64, SearchResult)> = Vec::new();

    const USN_REASON_FILE_CREATE: u32 = 0x100;
    const USN_REASON_FILE_DELETE: u32 = 0x200;
    const USN_REASON_RENAME_OLD_NAME: u32 = 0x1000;
    const USN_REASON_RENAME_NEW_NAME: u32 = 0x2000;
    const USN_REASON_DATA_OVERWRITE: u32 = 0x01;
    const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x8000;

    const RECYCLE_BIN: &str = "$recycle.bin";
    const SYSTEM_VOL_INFO: &str = "system volume information";

    batch_parent_buf.clear();
    batch_name_buf.clear();
    for rec in &records {
        batch_parent_buf.insert(rec.fid, rec.parent_fid);
        batch_name_buf.insert(rec.fid, rec.file_name.clone());
    }

    let mut mft_reader =
        ntfs_mft::open_volume_handle(drive_letter).map(ntfs_mft::UsnMetadataReader::new);

    let entry_count = records.len();

    let mut added_fids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut removed_fids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    for entry in &records {
        let reason = entry.reason;
        let fid = entry.fid;

        if reason & USN_REASON_FILE_DELETE != 0 || reason & USN_REASON_RENAME_OLD_NAME != 0 {
            if removed_fids.insert(fid) {
                let del_path = match resolver.resolve_path(entry) {
                    Some(p) => p.to_string_lossy().to_string(),
                    None => {
                        match resolve_path_from_batch(
                            fid,
                            batch_parent_buf,
                            batch_name_buf,
                            drive_letter,
                        ) {
                            Some(p) => p.to_string_lossy().to_string(),
                            None => {
                                log::warn!(
                                    "[USN] resolve_path FAILED (removed) for fid={}, name={}, reason=0x{:x}",
                                    fid,
                                    entry.file_name.to_string_lossy(),
                                    reason
                                );
                                String::new()
                            }
                        }
                    }
                };
                removed.push((fid, del_path));
            }
            added_fids.remove(&fid);
        }

        if reason & USN_REASON_FILE_CREATE != 0 || reason & USN_REASON_RENAME_NEW_NAME != 0 {
            if !added_fids.insert(fid) {
                continue;
            }
            let path = match resolver.resolve_path(entry) {
                Some(p) => p,
                None => {
                    match resolve_path_from_batch(
                        fid,
                        batch_parent_buf,
                        batch_name_buf,
                        drive_letter,
                    ) {
                        Some(p) => p,
                        None => {
                            log::warn!(
                                "[USN] resolve_path FAILED for fid={}, name={}, reason=0x{:x}, is_dir={}",
                                fid,
                                entry.file_name.to_string_lossy(),
                                reason,
                                entry.is_dir()
                            );
                            continue;
                        }
                    }
                }
            };

            let skip = path.components().any(|comp| {
                let s = comp.as_os_str().to_string_lossy();
                let sl = s.to_lowercase();
                if sl == RECYCLE_BIN {
                    return true;
                }
                if !include_system_files && sl == SYSTEM_VOL_INFO {
                    return true;
                }
                false
            });
            if skip {
                continue;
            }

            if !include_hidden_files && entry.is_hidden() {
                continue;
            }
            if entry.file_name.to_string_lossy().starts_with('$') {
                continue;
            }

            let name: CompactString =
                CompactString::from(entry.file_name.to_string_lossy().as_ref());
            let path_str: CompactString = CompactString::from(path.to_string_lossy().as_ref());
            let meta = mft_reader
                .as_mut()
                .map(|r| r.get_file_metadata(fid, &path))
                .unwrap_or(ntfs_mft::FileMetadata {
                    size: 0,
                    modified_time: 0,
                    is_directory: entry.is_dir(),
                });
            added.push(SearchResult {
                file_id: fid as u32,
                name,
                path: path_str,
                size: meta.size,
                modified_time: meta.modified_time,
                is_directory: meta.is_directory,
            });
        }

        if reason & USN_REASON_DATA_OVERWRITE != 0 || reason & USN_REASON_BASIC_INFO_CHANGE != 0 {
            let path = match resolver.resolve_path(entry) {
                Some(p) => p,
                None => {
                    match resolve_path_from_batch(
                        fid,
                        batch_parent_buf,
                        batch_name_buf,
                        drive_letter,
                    ) {
                        Some(p) => p,
                        None => {
                            log::warn!(
                                "[USN] resolve_path FAILED (update) for fid={}, name={}, reason=0x{:x}",
                                fid,
                                entry.file_name.to_string_lossy(),
                                reason
                            );
                            continue;
                        }
                    }
                }
            };

            let name: CompactString =
                CompactString::from(entry.file_name.to_string_lossy().as_ref());
            let path_str: CompactString = CompactString::from(path.to_string_lossy().as_ref());
            let meta = mft_reader
                .as_mut()
                .map(|r| r.get_file_metadata(fid, &path))
                .unwrap_or(ntfs_mft::FileMetadata {
                    size: 0,
                    modified_time: 0,
                    is_directory: entry.is_dir(),
                });
            updated.push((
                fid,
                SearchResult {
                    file_id: fid as u32,
                    name,
                    path: path_str,
                    size: meta.size,
                    modified_time: meta.modified_time,
                    is_directory: meta.is_directory,
                },
            ));
        }
    }

    log::info!(
        "[USN] Poll-{}: entries_read={}, added={}, removed={}, updated={}, new_last_usn={}",
        drive_letter,
        entry_count,
        added.len(),
        removed.len(),
        updated.len(),
        new_last_usn
    );

    if new_last_usn > effective_last_usn {
        *last_usn = new_last_usn;

        let now = Instant::now();
        if now.duration_since(*last_save_time) >= save_interval {
            save_volume_state(drive_letter, new_last_usn);
            *last_save_time = now;
        }
    }

    if added.is_empty() && removed.is_empty() && updated.is_empty() {
        log::debug!(
            "[USN] Poll-{}: no relevant changes after filtering, skipping notification",
            drive_letter
        );
        return;
    }

    let _ = resp_tx.send(UsnResponse::IncrementalResult {
        drive_letter,
        added,
        removed,
        updated,
        last_usn: new_last_usn,
    });
}
