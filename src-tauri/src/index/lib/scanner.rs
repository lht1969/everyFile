#![allow(dead_code)]

use std::io::{self, Read, Seek, SeekFrom};

const NTFS_BLOCK_SIZE: u64 = 512;
const ATTR_DATA: u32 = 0x80;
const ATTR_STANDARD_INFO: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_END: u32 = 0xFFFF_FFFF;

const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x02;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x04;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
const FILE_ATTRIBUTE_SPARSE: u32 = 0x200;
const FILE_ATTRIBUTE_REPARSE: u32 = 0x400;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;

pub struct DataRun {
    pub lcn: u64,
    pub length: u64,
    pub is_sparse: bool,
}

pub struct ScanResult {
    pub record_number: u64,
    pub parent_record: u64,
    pub is_directory: bool,
    pub name: String,
    pub size: u64,
    pub mtime: Option<u64>,
    pub ctime: Option<u64>,
    pub atime: Option<u64>,
    pub attributes: u32,
}

#[allow(dead_code)]
pub struct ScanOutput {
    pub all_records: Vec<ScanResult>,
    pub total_records: u64,
    pub files: u64,
    pub dirs: u64,
    pub skip_no_signature: u64,
    pub skip_fixup_fail: u64,
    pub skip_inactive: u64,
    pub total_ads: u64,
    pub total_hard_links: u64,
    pub no_timestamp: u64,
}

/// 流式扫描的统计信息（不包含 all_records，内存占用恒定）
///
/// 与 ScanOutput 的区别：
/// - 不收集 all_records: Vec<ScanResult>，避免 ~260MB 内存峰值（221万文件场景）
/// - 不处理 pending_ext（$ATTRIBUTE_LIST 扩展记录的 size 更新）
///   影响范围：极少数 NTFS 稀疏/大文件 size 显示为 0，对搜索功能影响可接受
pub struct ScanStats {
    pub total_records: u64,
    pub files: u64,
    pub dirs: u64,
    pub skip_no_signature: u64,
    pub skip_fixup_fail: u64,
    pub skip_inactive: u64,
    pub total_ads: u64,
    pub total_hard_links: u64,
    pub no_timestamp: u64,
}

pub struct MftScanner {
    pub data_runs: Vec<DataRun>,
    pub cluster_size: u64,
    pub file_record_size: u64,
}

impl MftScanner {
    pub fn new(ntfs: &ntfs::Ntfs, reader: &mut (impl Read + Seek)) -> io::Result<Self> {
        let cluster_size = ntfs.cluster_size() as u64;
        let file_record_size = ntfs.file_record_size() as u64;
        let mft_pos = ntfs
            .mft_position()
            .value()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MFT position is null"))?
            .get();

        let mut record0 = vec![0u8; file_record_size as usize];
        reader.seek(SeekFrom::Start(mft_pos))?;
        reader.read_exact(&mut record0)?;

        apply_fixup(&mut record0, 0, file_record_size)?;

        let first_attr_offset = read_u16(&record0, 20) as usize;
        let mut offset = first_attr_offset;

        while offset + 64 <= record0.len() {
            let attr_type = read_u32(&record0, offset);
            let attr_len = read_u32(&record0, offset + 4) as usize;

            if attr_type == ATTR_END || attr_len == 0 || offset + attr_len > record0.len() {
                break;
            }

            if attr_type == ATTR_DATA && record0[offset + 8] == 1 {
                let data_runs_offset = read_u16(&record0, offset + 32) as usize;
                let runs_start = offset + data_runs_offset;
                let runs_end = offset + attr_len;

                if runs_start <= runs_end && runs_end <= record0.len() {
                    let data_runs = parse_data_runs(&record0[runs_start..runs_end]);
                    return Ok(Self {
                        data_runs,
                        cluster_size,
                        file_record_size,
                    });
                }
            }

            offset += attr_len;
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "No non-resident $DATA attribute in MFT record 0",
        ))
    }

    pub fn scan(&mut self, reader: &mut (impl Read + Seek), max_records: u64) -> ScanOutput {
        let mut all_records = Vec::new();
        let mut total_records: u64 = 0;
        let mut files: u64 = 0;
        let mut dirs: u64 = 0;
        let mut consecutive_errors: u64 = 0;
        let mut record_buf = vec![0u8; self.file_record_size as usize];
        let mut total_ads: u64 = 0;
        let mut total_hard_links: u64 = 0;
        let mut skip_no_signature: u64 = 0;
        let mut skip_fixup_fail: u64 = 0;
        let mut skip_inactive: u64 = 0;
        let mut no_timestamp: u64 = 0;
        let mut pending_ext: Vec<(usize, Vec<u64>)> = Vec::new();

        for run in &self.data_runs {
            if total_records >= max_records {
                break;
            }

            let run_bytes = run.length * self.cluster_size;
            let records_in_run = run_bytes / self.file_record_size;

            if run.is_sparse {
                total_records += records_in_run;
                continue;
            }

            let run_start = run.lcn * self.cluster_size;
            if reader.seek(SeekFrom::Start(run_start)).is_err() {
                consecutive_errors += 1;
                if consecutive_errors > 100 {
                    break;
                }
                total_records += records_in_run;
                continue;
            }

            for _ in 0..records_in_run {
                if total_records >= max_records {
                    break;
                }

                if reader.read_exact(&mut record_buf).is_err() {
                    consecutive_errors += 1;
                    if consecutive_errors > 100 {
                        break;
                    }
                    total_records += 1;
                    continue;
                }
                consecutive_errors = 0;

                if &record_buf[0..4] != b"FILE" {
                    total_records += 1;
                    skip_no_signature += 1;
                    continue;
                }

                if apply_fixup(&mut record_buf, total_records, self.file_record_size).is_err() {
                    total_records += 1;
                    skip_fixup_fail += 1;
                    continue;
                }

                let flags = read_u16(&record_buf, 22);
                if (flags & 0x01) == 0 {
                    total_records += 1;
                    skip_inactive += 1;
                    continue;
                }

                let is_dir = (flags & 0x02) != 0;
                if is_dir {
                    dirs += 1;
                } else {
                    files += 1;
                }

                let mut name = extract_name(&record_buf, self.file_record_size as usize);
                if name == "<no name>" {
                    if let Some(wk_name) = well_known_name(total_records) {
                        name = wk_name.to_string();
                    } else {
                        name = format!("<Record#{}>", total_records);
                    }
                }
                let parent_record = extract_parent_record(&record_buf, self.file_record_size as usize);
                let size = extract_data_size_from_bytes(&record_buf, self.file_record_size as usize);
                if size == 0 && !is_dir {
                    let ext = extract_data_extension_records(&record_buf, self.file_record_size as usize);
                    if !ext.is_empty() {
                        let result_idx = all_records.len();
                        pending_ext.push((result_idx, ext));
                    }
                }
                let mtime = extract_mtime(&record_buf, self.file_record_size as usize);
                let ctime = extract_ctime(&record_buf, self.file_record_size as usize);
                let atime = extract_atime(&record_buf, self.file_record_size as usize);
                if mtime.is_none() && ctime.is_none() && atime.is_none() {
                    no_timestamp += 1;
                }
                let attributes = extract_attributes(&record_buf, self.file_record_size as usize);
                let ads = count_ads(&record_buf, self.file_record_size as usize);
                total_ads += ads;
                let links = count_hard_links(&record_buf, self.file_record_size as usize);
                total_hard_links += links;

                all_records.push(ScanResult {
                    record_number: total_records,
                    parent_record,
                    is_directory: is_dir,
                    name,
                    size,
                    mtime,
                    ctime,
                    atime,
                    attributes,
                });

                total_records += 1;
            }
        }

        if !pending_ext.is_empty() {
            resolve_pending_sizes(
                reader,
                &self.data_runs,
                self.file_record_size,
                self.cluster_size,
                &pending_ext,
                &mut all_records,
            );
        }

        ScanOutput {
            all_records,
            total_records,
            files,
            dirs,
            skip_no_signature,
            skip_fixup_fail,
            skip_inactive,
            total_ads,
            total_hard_links,
            no_timestamp,
        }
    }

    /// 流式扫描：对每条有效的 MFT 记录调用 callback，不收集到 Vec
    ///
    /// 内存优势：恒定占用（仅 record_buf ~1MB），不随文件数量增长
    /// 对比 scan() 方法在 221万文件下 ~260MB 的 all_records，显著降低峰值内存
    ///
    /// 返回值：(ScanStats, pending_sizes)
    /// - pending_sizes: HashMap<record_number, real_size>
    ///   对于主记录 size=0 且有 $ATTRIBUTE_LIST 的文件，通过读取扩展 MFT 记录获取真实 size
    ///   调用方需用此 map 更新 FileEntry 中 size=0 的条目
    ///
    /// callback 内可进行路径解析、FileEntry 构建等操作
    /// callback 是 FnMut，允许修改捕获的变量（如 dir_map、files 等）
    pub fn scan_streaming<F: FnMut(&ScanResult)>(
        &mut self,
        reader: &mut (impl Read + Seek),
        max_records: u64,
        callback: &mut F,
    ) -> (ScanStats, std::collections::HashMap<u64, u64>) {
        let mut total_records: u64 = 0;
        let mut files: u64 = 0;
        let mut dirs: u64 = 0;
        let mut consecutive_errors: u64 = 0;
        let mut record_buf = vec![0u8; self.file_record_size as usize];
        let mut total_ads: u64 = 0;
        let mut total_hard_links: u64 = 0;
        let mut skip_no_signature: u64 = 0;
        let mut skip_fixup_fail: u64 = 0;
        let mut skip_inactive: u64 = 0;
        let mut no_timestamp: u64 = 0;
        // pending_ext: (record_number, ext_record_nums)
        // 收集主记录 size=0 且有 $ATTRIBUTE_LIST 的文件，扫描结束后统一读取扩展记录获取真实 size
        let mut pending_ext: Vec<(u64, Vec<u64>)> = Vec::new();

        for run in &self.data_runs {
            if total_records >= max_records {
                break;
            }

            let run_bytes = run.length * self.cluster_size;
            let records_in_run = run_bytes / self.file_record_size;

            if run.is_sparse {
                total_records += records_in_run;
                continue;
            }

            let run_start = run.lcn * self.cluster_size;
            if reader.seek(SeekFrom::Start(run_start)).is_err() {
                consecutive_errors += 1;
                if consecutive_errors > 100 {
                    break;
                }
                total_records += records_in_run;
                continue;
            }

            for _ in 0..records_in_run {
                if total_records >= max_records {
                    break;
                }

                if reader.read_exact(&mut record_buf).is_err() {
                    consecutive_errors += 1;
                    if consecutive_errors > 100 {
                        break;
                    }
                    total_records += 1;
                    continue;
                }
                consecutive_errors = 0;

                if &record_buf[0..4] != b"FILE" {
                    total_records += 1;
                    skip_no_signature += 1;
                    continue;
                }

                if apply_fixup(&mut record_buf, total_records, self.file_record_size).is_err() {
                    total_records += 1;
                    skip_fixup_fail += 1;
                    continue;
                }

                let flags = read_u16(&record_buf, 22);
                if (flags & 0x01) == 0 {
                    total_records += 1;
                    skip_inactive += 1;
                    continue;
                }

                let is_dir = (flags & 0x02) != 0;
                if is_dir {
                    dirs += 1;
                } else {
                    files += 1;
                }

                let mut name = extract_name(&record_buf, self.file_record_size as usize);
                if name == "<no name>" {
                    if let Some(wk_name) = well_known_name(total_records) {
                        name = wk_name.to_string();
                    } else {
                        name = format!("<Record#{}>", total_records);
                    }
                }
                let parent_record = extract_parent_record(&record_buf, self.file_record_size as usize);
                let size = extract_data_size_from_bytes(&record_buf, self.file_record_size as usize);
                // 主记录 size=0 且非目录：可能是有 $ATTRIBUTE_LIST 的大文件
                // 收集扩展记录号，扫描结束后批量读取以获取真实 size
                if size == 0 && !is_dir {
                    let ext = extract_data_extension_records(&record_buf, self.file_record_size as usize);
                    if !ext.is_empty() {
                        pending_ext.push((total_records, ext));
                    }
                }
                let mtime = extract_mtime(&record_buf, self.file_record_size as usize);
                let ctime = extract_ctime(&record_buf, self.file_record_size as usize);
                let atime = extract_atime(&record_buf, self.file_record_size as usize);
                if mtime.is_none() && ctime.is_none() && atime.is_none() {
                    no_timestamp += 1;
                }
                let attributes = extract_attributes(&record_buf, self.file_record_size as usize);
                let ads = count_ads(&record_buf, self.file_record_size as usize);
                total_ads += ads;
                let links = count_hard_links(&record_buf, self.file_record_size as usize);
                total_hard_links += links;

                // 构造临时 ScanResult 并调用 callback
                // 注意：name 所有权转移给 record，callback 结束后 record 被 drop
                let record = ScanResult {
                    record_number: total_records,
                    parent_record,
                    is_directory: is_dir,
                    name,
                    size,
                    mtime,
                    ctime,
                    atime,
                    attributes,
                };
                callback(&record);

                total_records += 1;
            }
        }

        // 扫描结束后，批量读取扩展 MFT 记录获取真实 size
        // 返回 HashMap<record_number, real_size> 供调用方更新 FileEntry
        let pending_sizes = if !pending_ext.is_empty() {
            log::info!(
                "scan_streaming: resolving {} pending_ext entries for real sizes",
                pending_ext.len()
            );
            resolve_pending_sizes_streaming(
                reader,
                &self.data_runs,
                self.file_record_size,
                self.cluster_size,
                &pending_ext,
            )
        } else {
            std::collections::HashMap::new()
        };

        (ScanStats {
            total_records,
            files,
            dirs,
            skip_no_signature,
            skip_fixup_fail,
            skip_inactive,
            total_ads,
            total_hard_links,
            no_timestamp,
        }, pending_sizes)
    }
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn read_unsigned_le(data: &[u8]) -> u64 {
    let mut value: u64 = 0;
    for (i, &byte) in data.iter().enumerate() {
        value |= (byte as u64) << (i * 8);
    }
    value
}

fn read_signed_le(data: &[u8]) -> i64 {
    let byte_count = data.len();
    let mut value: i64 = 0;
    for (i, &byte) in data.iter().enumerate() {
        value |= (byte as i64) << (i * 8);
    }
    if byte_count < 8 {
        let shift = (8 - byte_count) * 8;
        value = (value << shift) >> shift;
    }
    value
}

fn apply_fixup(record: &mut [u8], record_number: u64, record_size: u64) -> io::Result<()> {
    if (record.len() as u64) < NTFS_BLOCK_SIZE {
        return Ok(());
    }

    let usn_offset = read_u16(record, 4) as usize;
    let us_count = read_u16(record, 6) as usize;

    if us_count <= 1 || usn_offset + 2 > record.len() {
        return Ok(());
    }

    let usn = [record[usn_offset], record[usn_offset + 1]];
    let array_start = usn_offset + 2;
    let fixup_count = us_count - 1;
    let expected_sectors = record_size / NTFS_BLOCK_SIZE;

    if fixup_count as u64 != expected_sectors {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Record {}: fixup count {} doesn't match expected sectors {}",
                record_number, fixup_count, expected_sectors
            ),
        ));
    }

    for i in 0..fixup_count {
        let sector_end = ((i + 1) * NTFS_BLOCK_SIZE as usize) - 2;

        if sector_end + 2 > record.len() || array_start + i * 2 + 2 > record.len() {
            break;
        }

        if record[sector_end] != usn[0] || record[sector_end + 1] != usn[1] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Record {}: USN mismatch at sector {}", record_number, i),
            ));
        }

        record[sector_end] = record[array_start + i * 2];
        record[sector_end + 1] = record[array_start + i * 2 + 1];
    }

    Ok(())
}

fn parse_data_runs(data: &[u8]) -> Vec<DataRun> {
    let mut runs = Vec::new();
    let mut offset = 0;
    let mut prev_lcn: i64 = 0;

    while offset < data.len() {
        let header = data[offset];
        offset += 1;

        if header == 0 {
            break;
        }

        let len_size = (header & 0x0F) as usize;
        let off_size = ((header >> 4) & 0x0F) as usize;

        if len_size == 0 || offset + len_size > data.len() {
            break;
        }

        let run_length = read_unsigned_le(&data[offset..offset + len_size]);
        offset += len_size;

        if run_length == 0 {
            continue;
        }

        if offset + off_size > data.len() {
            break;
        }

        let lcn_offset = read_signed_le(&data[offset..offset + off_size]);
        offset += off_size;

        if lcn_offset == 0 {
            runs.push(DataRun {
                lcn: 0,
                length: run_length,
                is_sparse: true,
            });
        } else {
            prev_lcn += lcn_offset;
            runs.push(DataRun {
                lcn: prev_lcn as u64,
                length: run_length,
                is_sparse: false,
            });
        }
    }

    runs
}

/// Read an arbitrary MFT record by number from the data runs (with fixup applied)
pub fn read_record(
    reader: &mut (impl Read + Seek),
    data_runs: &[DataRun],
    file_record_size: u64,
    cluster_size: u64,
    target_record: u64,
) -> io::Result<Vec<u8>> {
    let mut record_number: u64 = 0;

    for run in data_runs {
        let records_in_run = (run.length * cluster_size) / file_record_size;

        if run.is_sparse {
            record_number += records_in_run;
            continue;
        }

        if target_record < record_number + records_in_run {
            let offset_in_run = target_record - record_number;
            let file_offset = run.lcn * cluster_size + offset_in_run * file_record_size;

            let mut record = vec![0u8; file_record_size as usize];
            reader.seek(SeekFrom::Start(file_offset))?;
            reader.read_exact(&mut record)?;
            apply_fixup(&mut record, target_record, file_record_size)?;
            return Ok(record);
        }

        record_number += records_in_run;
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("Record {} not found in data runs", target_record),
    ))
}

/// Parse $ATTRIBUTE_LIST entries, returning (attr_type, mft_record_number) for each entry
fn extract_attribute_list(record: &[u8], record_size: usize) -> Vec<(u32, u64)> {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;
    let mut entries = Vec::new();

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len == 0 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_ATTRIBUTE_LIST && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_len = read_u32(record, offset + 16) as usize;
            let value_start = offset + value_offset;
            let value_end = value_start + value_len;

            if value_end > record.len() {
                break;
            }

            let mut pos = value_start;
            while pos + 24 <= value_end {
                let entry_type = read_u32(record, pos);
                if entry_type == ATTR_END {
                    break;
                }
                let entry_len = read_u16(record, pos + 4) as usize;
                if entry_len < 24 || pos + entry_len > value_end {
                    break;
                }
                let mft_ref = read_u64(record, pos + 16);
                let record_num = mft_ref & 0x0000_FFFF_FFFF_FFFF;
                entries.push((entry_type, record_num));
                pos += entry_len;
            }
            break;
        }

        offset += attr_len;
    }

    entries
}

fn extract_parent_record(record: &[u8], record_size: usize) -> u64 {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len < 24 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_FILE_NAME && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_start = offset + value_offset;

            if value_start + 8 <= record.len() {
                let parent = read_u64(record, value_start) & 0x0000_FFFF_FFFF_FFFF;
                if parent != 0 {
                    return parent;
                }
            }
        }

        offset += attr_len;
    }

    0
}

fn well_known_name(record_number: u64) -> Option<&'static str> {
    match record_number {
        0 => Some("$MFT"),
        1 => Some("$MFTMirr"),
        2 => Some("$LogFile"),
        3 => Some("$Volume"),
        4 => Some("$AttrDef"),
        5 => Some("$"),
        6 => Some("$Bitmap"),
        7 => Some("$Boot"),
        8 => Some("$BadClus"),
        9 => Some("$Secure"),
        10 => Some("$UpCase"),
        11 => Some("$Extend"),
        _ => None,
    }
}

fn extract_name(record: &[u8], record_size: usize) -> String {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;
    let mut best_name = String::new();
    let mut best_namespace: u8 = 0xFF;

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len < 24 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_FILE_NAME && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_start = offset + value_offset;

            if value_start + 66 <= record.len() {
                let name_len = record[value_start + 64] as usize;
                let namespace = record[value_start + 65];
                let name_start = value_start + 66;
                let name_end = name_start + name_len * 2;

                if name_end <= record.len() && name_len > 0 {
                    let name_u16: Vec<u16> = record[name_start..name_end]
                        .chunks(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    let name = String::from_utf16_lossy(&name_u16);

                    if !name.is_empty() && namespace < best_namespace {
                        best_namespace = namespace;
                        best_name = name;
                    }
                }
            }
        }

        offset += attr_len;
    }

    if best_name.is_empty() {
        "<no name>".to_string()
    } else {
        best_name
    }
}

pub fn extract_data_size(
    record: &[u8],
    record_size: usize,
    reader: Option<&mut (impl Read + Seek)>,
    data_runs: Option<&[DataRun]>,
    file_record_size: u64,
    cluster_size: u64,
) -> u64 {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len == 0 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_DATA {
            let non_resident = record[offset + 8];
            if non_resident == 0 {
                let value_len = read_u32(record, offset + 16) as u64;
                return value_len;
            } else {
                return read_u64(record, offset + 48);
            }
        }

        offset += attr_len;
    }

    // $DATA not found directly — try $ATTRIBUTE_LIST fallback
    if let (Some(reader), Some(data_runs)) = (reader, data_runs) {
        let entries = extract_attribute_list(record, record_size);
        let mut seen = std::collections::HashSet::new();
        for (entry_type, record_num) in entries {
            if entry_type != ATTR_DATA {
                continue;
            }
            if !seen.insert(record_num) {
                continue;
            }
            if let Ok(ext_record) = read_record(reader, data_runs, file_record_size, cluster_size, record_num) {
                let size = extract_data_size_from_bytes(&ext_record, ext_record.len());
                if size > 0 {
                    return size;
                }
            }
        }
    }

    0
}

/// Extract $DATA extension record numbers from $ATTRIBUTE_LIST (no I/O)
fn extract_data_extension_records(record: &[u8], record_size: usize) -> Vec<u64> {
    let entries = extract_attribute_list(record, record_size);
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for (entry_type, record_num) in entries {
        if entry_type == ATTR_DATA && seen.insert(record_num) {
            result.push(record_num);
        }
    }
    result
}

/// Compute disk byte offset for a given MFT record number
fn record_disk_offset(data_runs: &[DataRun], file_record_size: u64, cluster_size: u64, target_record: u64) -> Option<u64> {
    let mut record_number: u64 = 0;
    for run in data_runs {
        let records_in_run = (run.length * cluster_size) / file_record_size;
        if run.is_sparse {
            record_number += records_in_run;
            continue;
        }
        if target_record < record_number + records_in_run {
            let offset_in_run = target_record - record_number;
            return Some(run.lcn * cluster_size + offset_in_run * file_record_size);
        }
        record_number += records_in_run;
    }
    None
}

/// 流式版本的 resolve_pending_sizes
///
/// 与 resolve_pending_sizes 的区别：
/// - 输入: pending 为 (record_number, ext_record_nums)，而非 (result_idx, ext_record_nums)
/// - 输出: 返回 HashMap<record_number, real_size>，而非直接修改 results[result_idx]
/// - 原因: scan_streaming 不收集 all_records，无法通过 result_idx 索引
///   调用方通过 record_number 匹配 FileEntry.file_id 来更新 size
fn resolve_pending_sizes_streaming(
    reader: &mut (impl Read + Seek),
    data_runs: &[DataRun],
    file_record_size: u64,
    cluster_size: u64,
    pending: &[(u64, Vec<u64>)],
) -> std::collections::HashMap<u64, u64> {
    // 收集所有扩展记录的磁盘偏移，按偏移排序以实现顺序 I/O
    let mut offsets: Vec<(u64, u64)> = Vec::new(); // (disk_offset, ext_record_number)
    let mut total_ext_records = 0usize;
    for &(_, ref ext_records) in pending {
        total_ext_records += ext_records.len();
        for &rn in ext_records {
            if let Some(offset) = record_disk_offset(data_runs, file_record_size, cluster_size, rn) {
                offsets.push((offset, rn));
            }
        }
    }
    offsets.sort_unstable_by_key(|&(offset, _)| offset);

    log::info!(
        "resolve_pending_sizes_streaming: {} pending entries, {} ext_records total, {} offsets resolved",
        pending.len(), total_ext_records, offsets.len()
    );

    // 读取每个扩展记录，提取 $DATA 属性的 size
    // ext_sizes: ext_record_number → size
    let mut ext_sizes: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut read_failures = 0usize;
    let mut zero_size_ext = 0usize;
    let mut record_buf = vec![0u8; file_record_size as usize];
    for &(offset, record_number) in &offsets {
        if reader.seek(SeekFrom::Start(offset)).is_err() {
            read_failures += 1;
            continue;
        }
        if reader.read_exact(&mut record_buf).is_err() {
            read_failures += 1;
            continue;
        }
        if apply_fixup(&mut record_buf, record_number, file_record_size).is_err() {
            read_failures += 1;
            continue;
        }
        let size = extract_data_size_from_bytes(&record_buf, record_buf.len());
        if size > 0 {
            ext_sizes.insert(record_number, size);
        } else {
            zero_size_ext += 1;
        }
    }

    log::info!(
        "resolve_pending_sizes_streaming: read {} ext records, {} with size>0, {} with size=0, {} read failures",
        offsets.len(), ext_sizes.len(), zero_size_ext, read_failures
    );

    // 构建 record_number → real_size 映射
    // 对于每个 pending 条目，遍历其扩展记录，取第一个 size>0 的作为真实 size
    let mut result: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    for &(record_number, ref ext_records) in pending {
        for &rn in ext_records {
            if let Some(&size) = ext_sizes.get(&rn) {
                if size > 0 {
                    result.insert(record_number, size);
                    break;
                }
            }
        }
    }

    log::info!(
        "resolve_pending_sizes_streaming: resolved {} real sizes from {} pending entries",
        result.len(), pending.len()
    );

    result
}

/// Batch-resolve sizes for records that have $ATTRIBUTE_LIST.
/// Reads all extension records sorted by disk offset for sequential I/O.
fn resolve_pending_sizes(
    reader: &mut (impl Read + Seek),
    data_runs: &[DataRun],
    file_record_size: u64,
    cluster_size: u64,
    pending: &[(usize, Vec<u64>)],
    results: &mut [ScanResult],
) {
    let mut offsets: Vec<(u64, u64)> = Vec::new();
    for (_, record_nums) in pending {
        for &rn in record_nums {
            if let Some(offset) = record_disk_offset(data_runs, file_record_size, cluster_size, rn) {
                offsets.push((offset, rn));
            }
        }
    }
    offsets.sort_unstable_by_key(|&(offset, _)| offset);

    let mut sizes: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut record_buf = vec![0u8; file_record_size as usize];
    for &(offset, record_number) in &offsets {
        if reader.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        if reader.read_exact(&mut record_buf).is_err() {
            continue;
        }
        if apply_fixup(&mut record_buf, record_number, file_record_size).is_err() {
            continue;
        }
        let size = extract_data_size_from_bytes(&record_buf, record_buf.len());
        if size > 0 {
            sizes.insert(record_number, size);
        }
    }

    for (result_idx, record_nums) in pending {
        for &rn in record_nums {
            if let Some(&size) = sizes.get(&rn){
                if size > 0 {   
                    results[*result_idx].size = size;
                    break;
                }
            }
        }
    }
}

/// Pure version of extract_data_size for already-loaded record bytes
fn extract_data_size_from_bytes(record: &[u8], record_size: usize) -> u64 {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len == 0 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_DATA {
            let non_resident = record[offset + 8];
            if non_resident == 0 {
                return read_u32(record, offset + 16) as u64;
            } else {
                return read_u64(record, offset + 48);
            }
        }

        offset += attr_len;
    }

    0
}

fn extract_mtime(record: &[u8], record_size: usize) -> Option<u64> {
    extract_si_time(record, record_size, 8).or_else(|| extract_fn_time(record, record_size, 16))
}

fn extract_ctime(record: &[u8], record_size: usize) -> Option<u64> {
    extract_si_time(record, record_size, 0).or_else(|| extract_fn_time(record, record_size, 8))
}

fn extract_atime(record: &[u8], record_size: usize) -> Option<u64> {
    extract_si_time(record, record_size, 24).or_else(|| extract_fn_time(record, record_size, 32))
}

fn extract_si_time(record: &[u8], record_size: usize, time_offset: usize) -> Option<u64> {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len == 0 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_STANDARD_INFO && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_start = offset + value_offset;

            if value_start + time_offset + 8 <= record.len() {
                return Some(read_u64(record, value_start + time_offset));
            }
        }

        offset += attr_len;
    }

    None
}

fn extract_fn_time(record: &[u8], record_size: usize, time_offset: usize) -> Option<u64> {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len == 0 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_FILE_NAME && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_start = offset + value_offset;

            if value_start + time_offset + 8 <= record.len() {
                return Some(read_u64(record, value_start + time_offset));
            }
        }

        offset += attr_len;
    }

    None
}

fn extract_attributes(record: &[u8], record_size: usize) -> u32 {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len == 0 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_STANDARD_INFO && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_start = offset + value_offset;

            if value_start + 40 <= record.len() {
                return read_u32(record, value_start + 32);
            }
        }

        offset += attr_len;
    }

    0
}

pub fn format_attributes(attrs: u32) -> String {
    let mut flags = Vec::new();
    if attrs & FILE_ATTRIBUTE_READONLY != 0 { flags.push("R"); }
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 { flags.push("H"); }
    if attrs & FILE_ATTRIBUTE_SYSTEM != 0 { flags.push("S"); }
    if attrs & FILE_ATTRIBUTE_ARCHIVE != 0 { flags.push("A"); }
    if attrs & FILE_ATTRIBUTE_NORMAL != 0 { flags.push("N"); }
    if attrs & FILE_ATTRIBUTE_TEMPORARY != 0 { flags.push("T"); }
    if attrs & FILE_ATTRIBUTE_SPARSE != 0 { flags.push("P"); }
    if attrs & FILE_ATTRIBUTE_REPARSE != 0 { flags.push("L"); }
    if attrs & FILE_ATTRIBUTE_COMPRESSED != 0 { flags.push("C"); }
    if attrs & FILE_ATTRIBUTE_ENCRYPTED != 0 { flags.push("E"); }
    if flags.is_empty() {
        "-".to_string()
    } else {
        flags.join("")
    }
}

fn count_ads(record: &[u8], record_size: usize) -> u64 {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;
    let mut count = 0u64;

    while offset + 12 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len < 24 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_DATA {
            let name_len = record[offset + 9] as usize;
            if name_len > 0 {
                count += 1;
            }
        }

        offset += attr_len;
    }

    count
}

/// Dump raw bytes of a specific MFT record for debugging
pub fn dump_record(
    reader: &mut (impl Read + Seek),
    data_runs: &[DataRun],
    file_record_size: u64,
    cluster_size: u64,
    target_record: u64,
) -> io::Result<Vec<u8>> {
    let mut record_number: u64 = 0;

    for run in data_runs {
        if run.is_sparse {
            let records_in_run = (run.length * cluster_size) / file_record_size;
            record_number += records_in_run;
            continue;
        }

        let run_start = run.lcn * cluster_size;
        let records_in_run = (run.length * cluster_size) / file_record_size;

        if target_record < record_number + records_in_run {
            let offset_in_run = target_record - record_number;
            let file_offset = run_start + offset_in_run * file_record_size;

            reader.seek(SeekFrom::Start(file_offset))?;
            let mut record = vec![0u8; file_record_size as usize];
            reader.read_exact(&mut record)?;
            let _ = apply_fixup(&mut record, target_record, file_record_size);
            return Ok(record);
        }

        record_number += records_in_run;
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("Record {} not found", target_record),
    ))
}

pub struct TimestampComparison {
    pub si_ctime: Option<u64>,
    pub si_mtime: Option<u64>,
    pub si_atime: Option<u64>,
    pub fn_ctime: Option<u64>,
    pub fn_mtime: Option<u64>,
    pub fn_atime: Option<u64>,
}

/// Compare timestamps from $STANDARD_INFORMATION vs $FILE_NAME attributes
pub fn compare_timestamps(record: &[u8]) -> TimestampComparison {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;
    let mut si_ctime = None;
    let mut si_mtime = None;
    let mut si_atime = None;
    let mut fn_mtime = None;
    let mut fn_ctime = None;
    let mut fn_atime = None;

    while offset + 8 <= record.len() {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len == 0 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_STANDARD_INFO && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_start = offset + value_offset;
            if value_start + 40 <= record.len() {
                si_ctime = Some(read_u64(record, value_start));
                si_mtime = Some(read_u64(record, value_start + 8));
                si_atime = Some(read_u64(record, value_start + 24));
            }
        }

        if attr_type == ATTR_FILE_NAME && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_start = offset + value_offset;
            if value_start + 40 <= record.len() && fn_mtime.is_none() {
                fn_ctime = Some(read_u64(record, value_start + 8));
                fn_mtime = Some(read_u64(record, value_start + 16));
                fn_atime = Some(read_u64(record, value_start + 32));
            }
        }

        offset += attr_len;
    }

    TimestampComparison {
        si_ctime,
        si_mtime,
        si_atime,
        fn_ctime,
        fn_mtime,
        fn_atime,
    }
}

fn count_hard_links(record: &[u8], record_size: usize) -> u64 {
    let first_attr_offset = read_u16(record, 20) as usize;
    let mut offset = first_attr_offset;
    let mut parents: Vec<u64> = Vec::new();

    while offset + 8 <= record.len().min(record_size) {
        let attr_type = read_u32(record, offset);
        let attr_len = read_u32(record, offset + 4) as usize;

        if attr_type == ATTR_END || attr_len < 24 || offset + attr_len > record.len() {
            break;
        }

        if attr_type == ATTR_FILE_NAME && record[offset + 8] == 0 {
            let value_offset = read_u16(record, offset + 20) as usize;
            let value_start = offset + value_offset;

            if value_start + 8 <= record.len() {
                let parent = read_u64(record, value_start) & 0x0000_FFFF_FFFF_FFFF;
                if !parents.contains(&parent) {
                    parents.push(parent);
                }
            }
        }

        offset += attr_len;
    }

    parents.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_runs_empty() {
        let runs = parse_data_runs(&[0]);
        assert!(runs.is_empty());
    }

    #[test]
    fn parse_data_runs_single() {
        let runs = parse_data_runs(&[0x21, 0x04, 0x08, 0x00]);
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].is_sparse);
        assert_eq!(runs[0].lcn, 8);
        assert_eq!(runs[0].length, 4);
    }

    #[test]
    fn parse_data_runs_sparse() {
        let runs = parse_data_runs(&[0x21, 0x04, 0x00, 0x00]);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].is_sparse);
        assert_eq!(runs[0].length, 4);
    }

    #[test]
    fn parse_data_runs_multiple() {
        let data = [
            0x21, 0x04, 0x08, 0x00, 0x11, 0x02, 0x03, 0x00,
        ];
        let runs = parse_data_runs(&data);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].lcn, 8);
        assert_eq!(runs[0].length, 4);
        assert_eq!(runs[1].lcn, 11);
        assert_eq!(runs[1].length, 2);
    }

    #[test]
    fn signed_le_positive() {
        assert_eq!(read_signed_le(&[0x01]), 1);
        assert_eq!(read_signed_le(&[0x08, 0x00]), 8);
    }

    #[test]
    fn signed_le_negative() {
        assert_eq!(read_signed_le(&[0xFF]), -1);
        assert_eq!(read_signed_le(&[0xFE, 0xFF]), -2);
    }

    /// Build a minimal fake MFT record (1024 bytes) with $STANDARD_INFORMATION
    /// and $FILE_NAME attributes, placing known timestamps at the expected offsets.
    fn build_fake_record(
        si_ctime: u64,
        si_mtime: u64,
        si_atime: u64,
        si_attrs: u32,
        fn_mtime: u64,
    ) -> Vec<u8> {
        let record_size = 1024usize;
        let mut record = vec![0u8; record_size];

        // MFT record header
        record[0..4].copy_from_slice(b"FILE");
        // Update sequence offset (usually 0x30 or 0x38)
        let usn_offset: u16 = 0x30;
        record[4..6].copy_from_slice(&usn_offset.to_le_bytes());
        // Update sequence count: 3 (2 sectors + 1 USN word)
        record[6..8].copy_from_slice(&3u16.to_le_bytes());
        // First attribute offset: 0x40
        let first_attr_offset: u16 = 0x40;
        record[20..22].copy_from_slice(&first_attr_offset.to_le_bytes());
        // Flags: 0x01 (in use) + 0x02 (directory) = 0x03
        record[22..24].copy_from_slice(&0x03u16.to_le_bytes());

        // USN at offset 0x30: value 0x0001 (must match last 2 bytes of each sector)
        // Sector 0 ends at byte 510, sector 1 ends at byte 1022.
        record[0x30] = 0x01;
        record[0x31] = 0x00;
        // Fixup array entries at 0x32 and 0x34
        record[0x32] = 0x01; // sector 0 replacement low byte
        record[0x33] = 0x00; // sector 0 replacement high byte
        record[0x34] = 0x02; // sector 1 replacement low byte
        record[0x35] = 0x00; // sector 1 replacement high byte

        // Place sentinel USN values at sector ends so fixup succeeds
        record[510] = 0x01;
        record[511] = 0x00;
        record[1022] = 0x01;
        record[1023] = 0x00;

        // --- $STANDARD_INFORMATION at first_attr_offset (0x40) ---
        let si_off = first_attr_offset as usize;
        record[si_off..si_off + 4].copy_from_slice(&ATTR_STANDARD_INFO.to_le_bytes()); // type
        let si_len: u32 = 96; // enough room
        record[si_off + 4..si_off + 8].copy_from_slice(&si_len.to_le_bytes()); // length
        record[si_off + 8] = 0; // non-resident = 0
        // value_offset at attr+20 = 0x18 (24)
        record[si_off + 20..si_off + 22].copy_from_slice(&0x18u16.to_le_bytes());
        // value_length at attr+16 = 48 (standard SI value size)
        record[si_off + 16..si_off + 20].copy_from_slice(&48u32.to_le_bytes());

        let val = si_off + 0x18;
        record[val..val + 8].copy_from_slice(&si_ctime.to_le_bytes());     // creation
        record[val + 8..val + 16].copy_from_slice(&si_mtime.to_le_bytes()); // modification
        record[val + 16..val + 24].copy_from_slice(&0u64.to_le_bytes());    // mft change
        record[val + 24..val + 32].copy_from_slice(&si_atime.to_le_bytes()); // access
        record[val + 32..val + 36].copy_from_slice(&si_attrs.to_le_bytes()); // attributes

        // --- $FILE_NAME after $STANDARD_INFORMATION ---
        let fn_off = si_off + si_len as usize;
        record[fn_off..fn_off + 4].copy_from_slice(&ATTR_FILE_NAME.to_le_bytes()); // type
        let fn_len: u32 = 128;
        record[fn_off + 4..fn_off + 8].copy_from_slice(&fn_len.to_le_bytes());
        record[fn_off + 8] = 0; // non-resident
        record[fn_off + 20..fn_off + 22].copy_from_slice(&0x18u16.to_le_bytes());
        record[fn_off + 16..fn_off + 20].copy_from_slice(&80u32.to_le_bytes());

        let fval = fn_off + 0x18;
        // Parent directory reference (record 5 = root)
        record[fval..fval + 8].copy_from_slice(&5u64.to_le_bytes());
        // File name modification time at +16
        record[fval + 16..fval + 24].copy_from_slice(&fn_mtime.to_le_bytes());
        // Name length at +64, namespace at +65
        record[fval + 64] = 4; // 4 UTF-16 chars
        record[fval + 65] = 0; // POSIX namespace
        // Name "test" in UTF-16LE at +66
        let name_bytes = "test".encode_utf16().collect::<Vec<u16>>();
        for (i, ch) in name_bytes.iter().enumerate() {
            record[fval + 66 + i * 2..fval + 68 + i * 2].copy_from_slice(&ch.to_le_bytes());
        }

        // --- End attribute ---
        let end_off = fn_off + fn_len as usize;
        record[end_off..end_off + 4].copy_from_slice(&ATTR_END.to_le_bytes());

        record
    }

    #[test]
    fn extract_si_timestamps_from_fake_record() {
        let ctime: u64 = 131_000_000_000_000_000; // ~2016
        let mtime: u64 = 132_000_000_000_000_000; // ~2019
        let atime: u64 = 133_000_000_000_000_000; // ~2022
        let attrs: u32 = 0x20; // ARCHIVE

        let record = build_fake_record(ctime, mtime, atime, attrs, 0);

        assert_eq!(extract_ctime(&record, 1024), Some(ctime));
        assert_eq!(extract_mtime(&record, 1024), Some(mtime));
        assert_eq!(extract_atime(&record, 1024), Some(atime));
        assert_eq!(extract_attributes(&record, 1024), attrs);
    }

    #[test]
    fn extract_fn_mtime_from_fake_record() {
        let fn_mtime: u64 = 134_000_000_000_000_000;
        let record = build_fake_record(0, 0, 0, 0, fn_mtime);

        let ts = compare_timestamps(&record);
        assert_eq!(ts.si_mtime, Some(0));
        assert_eq!(ts.fn_mtime, Some(fn_mtime));
    }

    #[test]
    fn compare_timestamps_returns_both_sources() {
        let c1: u64 = 100;
        let m1: u64 = 200;
        let a1: u64 = 300;
        let m2: u64 = 400;
        let record = build_fake_record(c1, m1, a1, 0, m2);

        let ts = compare_timestamps(&record);
        assert_eq!(ts.si_ctime, Some(c1));
        assert_eq!(ts.si_mtime, Some(m1));
        assert_eq!(ts.si_atime, Some(a1));
        assert_eq!(ts.fn_ctime, Some(0));
        assert_eq!(ts.fn_mtime, Some(m2));
        assert_eq!(ts.fn_atime, Some(0));
    }

    #[test]
    fn extract_attribute_list_parses_entries() {
        let record_size = 1024usize;
        let mut record = vec![0u8; record_size];
        record[0..4].copy_from_slice(b"FILE");
        let first_attr_offset: u16 = 0x40;
        record[20..22].copy_from_slice(&first_attr_offset.to_le_bytes());
        let usn_offset: u16 = 0x30;
        record[4..6].copy_from_slice(&usn_offset.to_le_bytes());
        record[6..8].copy_from_slice(&3u16.to_le_bytes());
        record[0x30] = 0x01; record[0x31] = 0x00;
        record[0x32] = 0x01; record[0x33] = 0x00;
        record[0x34] = 0x02; record[0x35] = 0x00;
        record[510] = 0x01; record[511] = 0x00;
        record[1022] = 0x01; record[1023] = 0x00;

        let attr_off = first_attr_offset as usize;
        record[attr_off..attr_off + 4].copy_from_slice(&ATTR_ATTRIBUTE_LIST.to_le_bytes());
        let attr_len: u32 = 128;
        record[attr_off + 4..attr_off + 8].copy_from_slice(&attr_len.to_le_bytes());
        record[attr_off + 8] = 0; // resident
        record[attr_off + 16..attr_off + 20].copy_from_slice(&96u32.to_le_bytes()); // value_length
        record[attr_off + 20..attr_off + 22].copy_from_slice(&0x18u16.to_le_bytes()); // value_offset

        let val = attr_off + 0x18;

        // Entry 1: $STANDARD_INFO in record 100
        let e = val;
        record[e..e + 4].copy_from_slice(&ATTR_STANDARD_INFO.to_le_bytes());
        record[e + 4..e + 6].copy_from_slice(&32u16.to_le_bytes()); // entry_len (padded to 32)
        // mft_reference at e+16: record 100
        record[e + 16..e + 24].copy_from_slice(&100u64.to_le_bytes());

        // Entry 2: $DATA in record 200
        let e = val + 32;
        record[e..e + 4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        record[e + 4..e + 6].copy_from_slice(&32u16.to_le_bytes());
        record[e + 16..e + 24].copy_from_slice(&200u64.to_le_bytes());

        // Entry 3: $DATA again in record 300 (duplicate type, different record)
        let e = val + 64;
        record[e..e + 4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        record[e + 4..e + 6].copy_from_slice(&32u16.to_le_bytes());
        record[e + 16..e + 24].copy_from_slice(&300u64.to_le_bytes());

        let entries = extract_attribute_list(&record, record_size);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], (ATTR_STANDARD_INFO, 100));
        assert_eq!(entries[1], (ATTR_DATA, 200));
        assert_eq!(entries[2], (ATTR_DATA, 300));
    }

    #[test]
    fn extract_data_size_returns_zero_without_data() {
        let record = build_fake_record(100, 200, 300, 0x20, 400);
        let size = extract_data_size(&record, 1024, None::<&mut std::fs::File>, None, 0, 0);
        assert_eq!(size, 0);
    }
}
