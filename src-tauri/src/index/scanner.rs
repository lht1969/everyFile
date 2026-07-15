use std::io::{self, Read, Seek, SeekFrom};

const NTFS_BLOCK_SIZE: u64 = 512;
const ATTR_DATA: u32 = 0x80;
const ATTR_STANDARD_INFO: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
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

struct DataRun {
    lcn: u64,
    length: u64,
    is_sparse: bool,
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
}

pub struct MftScanner {
    data_runs: Vec<DataRun>,
    cluster_size: u64,
    file_record_size: u64,
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
                let size = extract_data_size(&record_buf, self.file_record_size as usize);
                let mtime = extract_mtime(&record_buf, self.file_record_size as usize);
                let ctime = extract_ctime(&record_buf, self.file_record_size as usize);
                let atime = extract_atime(&record_buf, self.file_record_size as usize);
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
        }
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

fn extract_data_size(record: &[u8], record_size: usize) -> u64 {
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

    0
}

fn extract_mtime(record: &[u8], record_size: usize) -> Option<u64> {
    extract_si_time(record, record_size, 8)
}

fn extract_ctime(record: &[u8], record_size: usize) -> Option<u64> {
    extract_si_time(record, record_size, 0)
}

fn extract_atime(record: &[u8], record_size: usize) -> Option<u64> {
    extract_si_time(record, record_size, 24)
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
}
