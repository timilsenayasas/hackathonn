// Parquet output via the parquet crate's low-level column writers.

use bytes::Bytes;
use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use std::sync::Arc;

pub struct StrCol {
    pub data: Vec<u8>,
    pub offs: Vec<u32>, // len = n+1
}
impl StrCol {
    pub fn new() -> Self {
        StrCol { data: Vec::new(), offs: vec![0] }
    }
    pub fn push(&mut self, s: &[u8]) {
        self.data.extend_from_slice(s);
        self.offs.push(self.data.len() as u32);
    }
    pub fn len(&self) -> usize {
        self.offs.len() - 1
    }
    fn to_bytearrays(&self) -> Vec<ByteArray> {
        let b = Bytes::from(self.data.clone());
        (0..self.len())
            .map(|i| ByteArray::from(b.slice(self.offs[i] as usize..self.offs[i + 1] as usize)))
            .collect()
    }
}

pub struct UserRows {
    pub user: StrCol,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue: Vec<i64>,
    pub topcat: Vec<i32>, // -1 = null
}
impl UserRows {
    pub fn new() -> Self {
        UserRows { user: StrCol::new(), day: vec![], sessions: vec![], events: vec![], revenue: vec![], topcat: vec![] }
    }
}

fn props() -> Arc<WriterProperties> {
    Arc::new(
        WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_compression(Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .set_data_page_size_limit(8 << 20)
            .set_max_row_group_size(usize::MAX)
            .build(),
    )
}

pub fn write_user(path: &std::path::Path, r: &UserRows, cats: &[Vec<u8>]) -> Result<(), Box<dyn std::error::Error>> {
    let schema = Arc::new(parse_message_type(
        "message user_daily {
            REQUIRED BYTE_ARRAY user_id (STRING);
            REQUIRED INT32 day (DATE);
            REQUIRED INT64 sessions;
            REQUIRED INT64 events;
            REQUIRED INT64 revenue_cents;
            OPTIONAL BYTE_ARRAY top_category (STRING);
        }",
    )?);
    let file = std::fs::File::create(path)?;
    let mut w = SerializedFileWriter::new(std::io::BufWriter::with_capacity(4 << 20, file), schema, props())?;
    let mut rg = w.next_row_group()?;
    let mut idx = 0;
    while let Some(mut col) = rg.next_column()? {
        match idx {
            0 => {
                let v = r.user.to_bytearrays();
                col.typed::<ByteArrayType>().write_batch(&v, None, None)?;
            }
            1 => {
                col.typed::<Int32Type>().write_batch(&r.day, None, None)?;
            }
            2 => {
                col.typed::<Int64Type>().write_batch(&r.sessions, None, None)?;
            }
            3 => {
                col.typed::<Int64Type>().write_batch(&r.events, None, None)?;
            }
            4 => {
                col.typed::<Int64Type>().write_batch(&r.revenue, None, None)?;
            }
            _ => {
                let cb: Vec<ByteArray> = cats.iter().map(|c| ByteArray::from(c.clone())).collect();
                let mut vals = Vec::new();
                let mut defs = Vec::with_capacity(r.topcat.len());
                for &t in &r.topcat {
                    if t >= 0 {
                        vals.push(cb[t as usize].clone());
                        defs.push(1i16);
                    } else {
                        defs.push(0i16);
                    }
                }
                col.typed::<ByteArrayType>().write_batch(&vals, Some(&defs), None)?;
            }
        }
        col.close()?;
        idx += 1;
    }
    rg.close()?;
    w.close()?;
    Ok(())
}

pub struct CampRow {
    pub campaign: Vec<u8>,
    pub channel: Vec<u8>,
    pub day: i32,
    pub sessions: i64,
    pub purchasing: i64,
    pub revenue: i64,
    pub assisted: i64,
}

pub fn write_campaign(path: &std::path::Path, rows: &[CampRow]) -> Result<(), Box<dyn std::error::Error>> {
    let schema = Arc::new(parse_message_type(
        "message campaign_daily {
            REQUIRED BYTE_ARRAY campaign_id (STRING);
            REQUIRED BYTE_ARRAY channel (STRING);
            REQUIRED INT32 day (DATE);
            REQUIRED INT64 sessions;
            REQUIRED INT64 purchasing_sessions;
            REQUIRED INT64 revenue_cents;
            REQUIRED INT64 assisted_sessions;
        }",
    )?);
    let file = std::fs::File::create(path)?;
    let mut w = SerializedFileWriter::new(std::io::BufWriter::new(file), schema, props())?;
    let mut rg = w.next_row_group()?;
    let mut idx = 0;
    while let Some(mut col) = rg.next_column()? {
        match idx {
            0 => {
                let v: Vec<ByteArray> = rows.iter().map(|r| ByteArray::from(r.campaign.clone())).collect();
                col.typed::<ByteArrayType>().write_batch(&v, None, None)?;
            }
            1 => {
                let v: Vec<ByteArray> = rows.iter().map(|r| ByteArray::from(r.channel.clone())).collect();
                col.typed::<ByteArrayType>().write_batch(&v, None, None)?;
            }
            2 => {
                let v: Vec<i32> = rows.iter().map(|r| r.day).collect();
                col.typed::<Int32Type>().write_batch(&v, None, None)?;
            }
            3 => {
                let v: Vec<i64> = rows.iter().map(|r| r.sessions).collect();
                col.typed::<Int64Type>().write_batch(&v, None, None)?;
            }
            4 => {
                let v: Vec<i64> = rows.iter().map(|r| r.purchasing).collect();
                col.typed::<Int64Type>().write_batch(&v, None, None)?;
            }
            5 => {
                let v: Vec<i64> = rows.iter().map(|r| r.revenue).collect();
                col.typed::<Int64Type>().write_batch(&v, None, None)?;
            }
            _ => {
                let v: Vec<i64> = rows.iter().map(|r| r.assisted).collect();
                col.typed::<Int64Type>().write_batch(&v, None, None)?;
            }
        }
        col.close()?;
        idx += 1;
    }
    rg.close()?;
    w.close()?;
    Ok(())
}
