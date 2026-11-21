use crate::pq::{Kind, Writer};

pub struct StrCol {
    pub data: Vec<u8>,
    pub offs: Vec<u32>,
}
impl StrCol {
    pub fn new() -> Self {
        StrCol { data: Vec::new(), offs: vec![0] }
    }
    #[inline]
    pub fn push(&mut self, s: &[u8]) {
        self.data.extend_from_slice(s);
        self.offs.push(self.data.len() as u32);
    }
    pub fn get(&self, i: usize) -> &[u8] {
        &self.data[self.offs[i] as usize..self.offs[i + 1] as usize]
    }
    pub fn len(&self) -> usize {
        self.offs.len() - 1
    }
}

// one name per person; each person's rows are consecutive
pub struct UserRows {
    pub names: StrCol,
    pub runs: Vec<u32>,
    pub day: Vec<u8>,
    pub sessions: Vec<u32>,
    pub events: Vec<u32>,
    pub revenue: Vec<u64>,
    pub topcat: Vec<u8>,
}
impl UserRows {
    pub fn new() -> Self {
        UserRows { names: StrCol::new(), runs: vec![], day: vec![], sessions: vec![], events: vec![], revenue: vec![], topcat: vec![] }
    }
    pub fn len(&self) -> usize {
        self.day.len()
    }
}

// dictionary of the distinct values of a small-integer column
fn small_dict(v: &[u32]) -> Option<(Vec<i64>, Vec<u32>)> {
    let mx = v.iter().copied().max().unwrap_or(0) as usize;
    if mx >= 1 << 20 {
        return None;
    }
    let mut slot = vec![u32::MAX; mx + 1];
    let mut dict = Vec::new();
    let idx = v
        .iter()
        .map(|&x| {
            let s = &mut slot[x as usize];
            if *s == u32::MAX {
                *s = dict.len() as u32;
                dict.push(x as i64);
            }
            *s
        })
        .collect();
    Some((dict, idx))
}

pub fn write_user(path: &std::path::Path, r: &UserRows, cats: &[Vec<u8>], day0: i64, nd: usize) -> std::io::Result<()> {
    let n = r.len();
    let mut w = Writer::create(path, n)?;
    let mut pidx: Vec<u32> = Vec::with_capacity(n);
    for (p, &k) in r.runs.iter().enumerate() {
        pidx.extend(std::iter::repeat(p as u32).take(k as usize));
    }
    w.dict_str("user_id", (0..r.names.len()).map(|i| r.names.get(i)), None, &|i| pidx[i])?;
    let days: Vec<i64> = (0..nd as i64).map(|d| day0 + d).collect();
    w.dict_int("day", Kind::Date, &days, &|i| r.day[i] as u32)?;
    for (name, col) in [("sessions", &r.sessions), ("events", &r.events)] {
        match small_dict(col) {
            Some((dict, idx)) => w.dict_int(name, Kind::I64, &dict, &|i| idx[i])?,
            None => w.plain_i64(name, col.iter().map(|&x| x as i64))?,
        }
    }
    w.plain_i64("revenue_cents", r.revenue.iter().map(|&x| x as i64))?;
    w.dict_str("top_category", cats.iter().map(|c| c.as_slice()), Some(&|i| r.topcat[i] != 255), &|i| r.topcat[i] as u32)?;
    w.finish()
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

pub fn write_campaign(path: &std::path::Path, rows: &[CampRow], day0: i64, nd: usize) -> std::io::Result<()> {
    let mut w = Writer::create(path, rows.len())?;
    w.plain_str("campaign_id", rows.iter().map(|r| r.campaign.as_slice()))?;
    w.plain_str("channel", rows.iter().map(|r| r.channel.as_slice()))?;
    let days: Vec<i64> = (0..nd as i64).map(|d| day0 + d).collect();
    w.dict_int("day", Kind::Date, &days, &|i| (rows[i].day as i64 - day0) as u32)?;
    w.plain_i64("sessions", rows.iter().map(|r| r.sessions))?;
    w.plain_i64("purchasing_sessions", rows.iter().map(|r| r.purchasing))?;
    w.plain_i64("revenue_cents", rows.iter().map(|r| r.revenue))?;
    w.plain_i64("assisted_sessions", rows.iter().map(|r| r.assisted))?;
    w.finish()
}
