use crate::pq::{self, Col};

pub struct StrCol {
    pub data: Vec<u8>,
    pub offs: Vec<u32>,
}
impl StrCol {
    pub fn new() -> Self {
        StrCol {
            data: Vec::new(),
            offs: vec![0],
        }
    }
    #[inline]
    pub fn push(&mut self, s: &[u8]) {
        self.data.extend_from_slice(s);
        self.offs.push(self.data.len() as u32);
    }
}

pub struct UserRows {
    pub user: StrCol,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue: Vec<i64>,
    pub topcat: Vec<i32>,
}
impl UserRows {
    pub fn new() -> Self {
        UserRows {
            user: StrCol::new(),
            day: vec![],
            sessions: vec![],
            events: vec![],
            revenue: vec![],
            topcat: vec![],
        }
    }
}

pub fn write_user(path: &std::path::Path, r: &UserRows, cats: &[Vec<u8>]) -> std::io::Result<()> {
    let n = r.day.len();
    let mut cdata: Vec<u8> = Vec::new();
    let mut coffs: Vec<u32> = Vec::with_capacity(n + 1);
    let mut valid: Vec<bool> = Vec::with_capacity(n);
    coffs.push(0);
    for &t in &r.topcat {
        if t >= 0 {
            cdata.extend_from_slice(&cats[t as usize]);
        }
        valid.push(t >= 0);
        coffs.push(cdata.len() as u32);
    }
    pq::write(
        path,
        n,
        &[
            Col::Str {
                name: "user_id",
                data: &r.user.data,
                offs: &r.user.offs,
                valid: None,
            },
            Col::I32 {
                name: "day",
                date: true,
                v: &r.day,
            },
            Col::I64 {
                name: "sessions",
                v: &r.sessions,
            },
            Col::I64 {
                name: "events",
                v: &r.events,
            },
            Col::I64 {
                name: "revenue_cents",
                v: &r.revenue,
            },
            Col::Str {
                name: "top_category",
                data: &cdata,
                offs: &coffs,
                valid: Some(&valid),
            },
        ],
    )
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

pub fn write_campaign(path: &std::path::Path, rows: &[CampRow]) -> std::io::Result<()> {
    let n = rows.len();
    let mut a = StrCol::new();
    let mut b = StrCol::new();
    for r in rows {
        a.push(&r.campaign);
        b.push(&r.channel);
    }
    let day: Vec<i32> = rows.iter().map(|r| r.day).collect();
    let s: Vec<i64> = rows.iter().map(|r| r.sessions).collect();
    let p: Vec<i64> = rows.iter().map(|r| r.purchasing).collect();
    let rv: Vec<i64> = rows.iter().map(|r| r.revenue).collect();
    let asx: Vec<i64> = rows.iter().map(|r| r.assisted).collect();
    pq::write(
        path,
        n,
        &[
            Col::Str {
                name: "campaign_id",
                data: &a.data,
                offs: &a.offs,
                valid: None,
            },
            Col::Str {
                name: "channel",
                data: &b.data,
                offs: &b.offs,
                valid: None,
            },
            Col::I32 {
                name: "day",
                date: true,
                v: &day,
            },
            Col::I64 { name: "sessions", v: &s },
            Col::I64 {
                name: "purchasing_sessions",
                v: &p,
            },
            Col::I64 {
                name: "revenue_cents",
                v: &rv,
            },
            Col::I64 {
                name: "assisted_sessions",
                v: &asx,
            },
        ],
    )
}
