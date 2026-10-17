//! One connection: the PostgreSQL v3 message flow.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use nucleus_kv::Kv;
use nucleus_sql::ast::Statement;
use nucleus_sql::{DataType, Error, OutColumn, Output, Session, TxnStatus, Value};

use crate::Config;

const PROTOCOL_V3: i32 = 196_608;
const SSL_REQUEST: i32 = 80_877_103;
const GSSENC_REQUEST: i32 = 80_877_104;
const CANCEL_REQUEST: i32 = 80_877_102;

// Type OIDs.
const BOOL: i32 = 16;
const INT8: i32 = 20;
const INT2: i32 = 21;
const INT4: i32 = 23;
const TEXT: i32 = 25;
const FLOAT4: i32 = 700;
const FLOAT8: i32 = 701;
const VARCHAR: i32 = 1043;
const UNKNOWN: i32 = 705;

fn oid(t: DataType) -> i32 {
    match t {
        DataType::Bool => BOOL,
        DataType::Int => INT8,
        DataType::Float => FLOAT8,
        DataType::Text => TEXT,
    }
}

fn type_of_oid(o: i32) -> Option<DataType> {
    Some(match o {
        BOOL => DataType::Bool,
        INT2 | INT4 | INT8 => DataType::Int,
        FLOAT4 | FLOAT8 => DataType::Float,
        TEXT | VARCHAR | UNKNOWN => DataType::Text,
        _ => return None,
    })
}

fn type_size(t: DataType) -> i16 {
    match t {
        DataType::Bool => 1,
        DataType::Int | DataType::Float => 8,
        DataType::Text => -1,
    }
}

/// An outgoing message being built.
struct Msg {
    buf: Vec<u8>,
}

impl Msg {
    fn new(tag: u8) -> Self {
        Self {
            buf: vec![tag, 0, 0, 0, 0],
        }
    }
    fn i16(mut self, v: i16) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn i32(mut self, v: i32) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn str(mut self, s: &str) -> Self {
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        self
    }
    fn bytes(mut self, b: &[u8]) -> Self {
        self.buf.extend_from_slice(b);
        self
    }
    fn u8(mut self, b: u8) -> Self {
        self.buf.push(b);
        self
    }
    fn finish(mut self) -> Vec<u8> {
        let len = (self.buf.len() - 1) as i32;
        self.buf[1..5].copy_from_slice(&len.to_be_bytes());
        self.buf
    }
}

/// An incoming message body being read.
struct Body<'a> {
    b: &'a [u8],
}

fn bad(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("protocol violation: {what}"),
    )
}

impl<'a> Body<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.b.len() < n {
            return Err(bad("message too short"));
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    fn i16(&mut self) -> io::Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn cstr(&mut self) -> io::Result<String> {
        let end = self
            .b
            .iter()
            .position(|&c| c == 0)
            .ok_or_else(|| bad("unterminated string"))?;
        let s = String::from_utf8(self.b[..end].to_vec()).map_err(|_| bad("string not UTF-8"))?;
        self.b = &self.b[end + 1..];
        Ok(s)
    }
}

struct Prepared {
    stmt: Option<Statement>,
    params: Vec<Option<DataType>>,
    columns: Option<Vec<OutColumn>>,
}

struct Portal {
    stmt: Option<Statement>,
    params: Vec<Value>,
    result_formats: Vec<i16>,
    columns: Option<Vec<OutColumn>>,
    /// Rows produced but not yet sent (after a suspended Execute), and the
    /// command tag to send once they are.
    pending: Option<(std::collections::VecDeque<Vec<Value>>, String)>,
}

struct Conn {
    out: BufWriter<TcpStream>,
    session: Session,
    prepared: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
}

/// Serves one client connection until it terminates.
pub fn handle(stream: TcpStream, kv: Arc<dyn Kv>, config: &Config, id: u32) -> io::Result<()> {
    let mut input = BufReader::new(stream.try_clone()?);
    let mut out = BufWriter::new(stream);

    // Startup, possibly after declined SSL / GSSAPI requests.
    let params = loop {
        let len = read_i32(&mut input)?;
        if !(8..=10_000).contains(&len) {
            return Err(bad("bad startup length"));
        }
        let mut body = vec![0u8; len as usize - 4];
        input.read_exact(&mut body)?;
        let code = i32::from_be_bytes(body[0..4].try_into().unwrap());
        match code {
            SSL_REQUEST | GSSENC_REQUEST => {
                out.write_all(b"N")?;
                out.flush()?;
            }
            CANCEL_REQUEST => return Ok(()),
            PROTOCOL_V3 => {
                let mut b = Body { b: &body[4..] };
                let mut params = HashMap::new();
                loop {
                    let k = b.cstr()?;
                    if k.is_empty() {
                        break;
                    }
                    params.insert(k, b.cstr()?);
                }
                break params;
            }
            v => {
                send_error(
                    &mut out,
                    &Error {
                        code: "08P01",
                        message: format!(
                            "unsupported frontend protocol {}.{}",
                            v >> 16,
                            v & 0xFFFF
                        ),
                    },
                )?;
                out.flush()?;
                return Ok(());
            }
        }
    };

    if let Some(pw) = &config.password {
        out.write_all(&Msg::new(b'R').i32(3).finish())?; // cleartext password
        out.flush()?;
        let (tag, body) = read_message(&mut input)?;
        let given = Body { b: &body }.cstr().unwrap_or_default();
        if tag != b'p' || given != *pw {
            let user = params.get("user").cloned().unwrap_or_default();
            send_error(
                &mut out,
                &Error {
                    code: "28P01",
                    message: format!("password authentication failed for user \"{user}\""),
                },
            )?;
            out.flush()?;
            return Ok(());
        }
    }
    out.write_all(&Msg::new(b'R').i32(0).finish())?;
    for (k, v) in [
        ("server_version", "16.0 (nucleus)"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
        ("TimeZone", "UTC"),
        ("IntervalStyle", "postgres"),
        ("is_superuser", "on"),
        (
            "application_name",
            params.get("application_name").map_or("", String::as_str),
        ),
    ] {
        out.write_all(&Msg::new(b'S').str(k).str(v).finish())?;
    }
    out.write_all(&Msg::new(b'K').i32(id as i32).i32(0x6e75_636c).finish())?;
    let mut conn = Conn {
        out,
        session: Session::new(kv),
        prepared: HashMap::new(),
        portals: HashMap::new(),
    };
    conn.ready()?;

    // After an error in the extended protocol, skip until Sync.
    let mut skipping = false;
    loop {
        let (tag, body) = read_message(&mut input)?;
        if skipping && tag != b'S' && tag != b'X' {
            continue;
        }
        let r = match tag {
            b'Q' => {
                let sql = Body { b: &body }.cstr()?;
                conn.simple_query(&sql)?;
                conn.ready()?;
                continue;
            }
            b'P' => conn.parse(&body),
            b'B' => conn.bind(&body),
            b'D' => conn.describe(&body),
            b'E' => conn.execute(&body),
            b'C' => conn.close(&body),
            b'S' => {
                skipping = false;
                conn.portals.retain(|name, _| !name.is_empty());
                conn.ready()?;
                continue;
            }
            b'H' => {
                conn.out.flush()?;
                continue;
            }
            b'X' => return Ok(()),
            other => Err(Error {
                code: "08P01",
                message: format!("unsupported message type {:?}", other as char),
            }),
        };
        match r {
            Ok(()) => {}
            Err(e) => {
                send_error(&mut conn.out, &e)?;
                skipping = true;
            }
        }
    }
}

fn read_i32(r: &mut impl Read) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

fn read_message(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let len = read_i32(r)?;
    if !(4..=(1 << 30)).contains(&len) {
        return Err(bad("bad message length"));
    }
    let mut body = vec![0u8; len as usize - 4];
    r.read_exact(&mut body)?;
    Ok((tag[0], body))
}

fn send_error(out: &mut impl Write, e: &Error) -> io::Result<()> {
    out.write_all(
        &Msg::new(b'E')
            .u8(b'S')
            .str("ERROR")
            .u8(b'V')
            .str("ERROR")
            .u8(b'C')
            .str(e.code)
            .u8(b'M')
            .str(&e.message)
            .u8(0)
            .finish(),
    )
}

fn proto(e: io::Error) -> Error {
    Error {
        code: "08P01",
        message: e.to_string(),
    }
}

/// The text form of a value, as PostgreSQL sends it.
fn text_value(v: &Value) -> String {
    v.render()
}

fn binary_value(v: &Value, ty: DataType) -> Vec<u8> {
    match (v, ty) {
        (Value::Bool(b), _) => vec![u8::from(*b)],
        (Value::Int(i), DataType::Float) => (*i as f64).to_be_bytes().to_vec(),
        (Value::Int(i), _) => i.to_be_bytes().to_vec(),
        (Value::Float(f), _) => f.to_be_bytes().to_vec(),
        (v, _) => text_value(v).into_bytes(),
    }
}

/// A parameter value from the wire.
fn decode_param(bytes: &[u8], format: i16, ty: Option<DataType>) -> Result<Value, Error> {
    let bad = |what: &str| Error {
        code: "22P03",
        message: format!("invalid binary parameter: {what}"),
    };
    if format == 1 {
        return Ok(match (ty, bytes.len()) {
            (Some(DataType::Bool), 1) => Value::Bool(bytes[0] != 0),
            (Some(DataType::Int), 8) => Value::Int(i64::from_be_bytes(bytes.try_into().unwrap())),
            (Some(DataType::Int), 4) => {
                Value::Int(i32::from_be_bytes(bytes.try_into().unwrap()).into())
            }
            (Some(DataType::Int), 2) => {
                Value::Int(i16::from_be_bytes(bytes.try_into().unwrap()).into())
            }
            (Some(DataType::Float), 8) => {
                Value::Float(f64::from_be_bytes(bytes.try_into().unwrap()))
            }
            (Some(DataType::Float), 4) => {
                Value::Float(f32::from_be_bytes(bytes.try_into().unwrap()).into())
            }
            (Some(DataType::Text) | None, _) => {
                Value::Text(String::from_utf8(bytes.to_vec()).map_err(|_| bad("text not UTF-8"))?)
            }
            _ => return Err(bad("unexpected length for its type")),
        });
    }
    let s = String::from_utf8(bytes.to_vec()).map_err(|_| Error {
        code: "22021",
        message: "parameter is not valid UTF-8".into(),
    })?;
    match ty {
        None | Some(DataType::Text) => Ok(Value::Text(s)),
        Some(t) => Value::Text(s).cast(t),
    }
}

fn command_tag(stmt: &Statement, out: &Output) -> String {
    match out {
        Output::Command(tag) => tag.clone(),
        Output::Rows { rows, .. } => match stmt {
            Statement::Insert { .. } => format!("INSERT 0 {}", rows.len()),
            Statement::Update { .. } => format!("UPDATE {}", rows.len()),
            Statement::Delete { .. } => format!("DELETE {}", rows.len()),
            Statement::Show(_) => "SHOW".into(),
            Statement::Explain(_) => "EXPLAIN".into(),
            _ => format!("SELECT {}", rows.len()),
        },
    }
}

impl Conn {
    fn ready(&mut self) -> io::Result<()> {
        let status = match self.session.status() {
            TxnStatus::Idle => b'I',
            TxnStatus::InBlock => b'T',
            TxnStatus::Failed => b'E',
        };
        self.out.write_all(&Msg::new(b'Z').u8(status).finish())?;
        self.out.flush()
    }

    fn row_description(&mut self, columns: &[OutColumn], formats: &[i16]) -> io::Result<()> {
        let mut m = Msg::new(b'T').i16(columns.len() as i16);
        for (i, c) in columns.iter().enumerate() {
            let f = format_for(formats, i);
            m = m
                .str(&c.name)
                .i32(0)
                .i16(0)
                .i32(oid(c.ty))
                .i16(type_size(c.ty))
                .i32(-1)
                .i16(f);
        }
        self.out.write_all(&m.finish())
    }

    fn data_row(
        &mut self,
        row: &[Value],
        columns: &[OutColumn],
        formats: &[i16],
    ) -> io::Result<()> {
        let mut m = Msg::new(b'D').i16(row.len() as i16);
        for (i, v) in row.iter().enumerate() {
            if v.is_null() {
                m = m.i32(-1);
                continue;
            }
            let bytes = if format_for(formats, i) == 1 {
                binary_value(v, columns.get(i).map_or(DataType::Text, |c| c.ty))
            } else {
                text_value(v).into_bytes()
            };
            m = m.i32(bytes.len() as i32).bytes(&bytes);
        }
        self.out.write_all(&m.finish())
    }

    fn simple_query(&mut self, sql: &str) -> io::Result<()> {
        let stmts = match nucleus_sql::parser::parse(sql) {
            Ok(s) => s,
            Err(e) => return send_error(&mut self.out, &e),
        };
        if stmts.is_empty() {
            return self.out.write_all(&Msg::new(b'I').finish());
        }
        // Several statements in one Query run in one implicit transaction
        // in PostgreSQL; here each runs on its own (autocommit), and the
        // first error stops the rest.
        for stmt in &stmts {
            match self.session.run(stmt, &[]) {
                Ok(out) => {
                    if let Output::Rows { columns, rows } = &out {
                        self.row_description(columns, &[])?;
                        for r in rows {
                            self.data_row(r, columns, &[])?;
                        }
                    }
                    let tag = command_tag(stmt, &out);
                    self.out.write_all(&Msg::new(b'C').str(&tag).finish())?;
                }
                Err(e) => return send_error(&mut self.out, &e),
            }
        }
        Ok(())
    }

    fn parse(&mut self, body: &[u8]) -> Result<(), Error> {
        let mut b = Body { b: body };
        let name = b.cstr().map_err(proto)?;
        let sql = b.cstr().map_err(proto)?;
        let n = b.i16().map_err(proto)?;
        let mut given = Vec::new();
        for _ in 0..n {
            given.push(b.i32().map_err(proto)?);
        }
        let mut stmts = nucleus_sql::parser::parse(&sql)?;
        if stmts.len() > 1 {
            return Err(Error::syntax(
                "cannot insert multiple commands into a prepared statement",
            ));
        }
        let stmt = stmts.pop();
        let (mut params, columns) = match &stmt {
            Some(s) => self.session.describe(s)?,
            None => (vec![], None),
        };
        if params.len() < given.len() {
            params.resize(given.len(), None);
        }
        for (i, o) in given.iter().enumerate() {
            if *o != 0 {
                params[i] = type_of_oid(*o).or(params[i]);
            }
        }
        self.prepared.insert(
            name,
            Prepared {
                stmt,
                params,
                columns,
            },
        );
        self.out.write_all(&Msg::new(b'1').finish()).map_err(proto)
    }

    fn bind(&mut self, body: &[u8]) -> Result<(), Error> {
        let mut b = Body { b: body };
        let portal = b.cstr().map_err(proto)?;
        let name = b.cstr().map_err(proto)?;
        let p = self.prepared.get(&name).ok_or_else(|| Error {
            code: "26000",
            message: format!("prepared statement \"{name}\" does not exist"),
        })?;
        let nf = b.i16().map_err(proto)? as usize;
        let mut formats = Vec::with_capacity(nf);
        for _ in 0..nf {
            formats.push(b.i16().map_err(proto)?);
        }
        let np = b.i16().map_err(proto)? as usize;
        let mut params = Vec::with_capacity(np);
        for i in 0..np {
            let len = b.i32().map_err(proto)?;
            if len < 0 {
                params.push(Value::Null);
                continue;
            }
            let bytes = b.take(len as usize).map_err(proto)?;
            params.push(decode_param(
                bytes,
                format_for(&formats, i),
                p.params.get(i).copied().flatten(),
            )?);
        }
        let nr = b.i16().map_err(proto)? as usize;
        let mut result_formats = Vec::with_capacity(nr);
        for _ in 0..nr {
            result_formats.push(b.i16().map_err(proto)?);
        }
        let portal_value = Portal {
            stmt: p.stmt.clone(),
            params,
            result_formats,
            columns: p.columns.clone(),
            pending: None,
        };
        self.portals.insert(portal, portal_value);
        self.out.write_all(&Msg::new(b'2').finish()).map_err(proto)
    }

    fn describe(&mut self, body: &[u8]) -> Result<(), Error> {
        let mut b = Body { b: body };
        let kind = b.take(1).map_err(proto)?[0];
        let name = b.cstr().map_err(proto)?;
        let (columns, formats, params) = match kind {
            b'S' => {
                let p = self.prepared.get(&name).ok_or_else(|| Error {
                    code: "26000",
                    message: format!("prepared statement \"{name}\" does not exist"),
                })?;
                (p.columns.clone(), vec![], Some(p.params.clone()))
            }
            _ => {
                let p = self.portals.get(&name).ok_or_else(|| Error {
                    code: "34000",
                    message: format!("portal \"{name}\" does not exist"),
                })?;
                (p.columns.clone(), p.result_formats.clone(), None)
            }
        };
        if let Some(params) = params {
            let mut m = Msg::new(b't').i16(params.len() as i16);
            for p in &params {
                m = m.i32(p.map_or(TEXT, oid));
            }
            self.out.write_all(&m.finish()).map_err(proto)?;
        }
        match columns {
            Some(c) => self.row_description(&c, &formats).map_err(proto),
            None => self.out.write_all(&Msg::new(b'n').finish()).map_err(proto),
        }
    }

    fn execute(&mut self, body: &[u8]) -> Result<(), Error> {
        let mut b = Body { b: body };
        let name = b.cstr().map_err(proto)?;
        let max = b.i32().map_err(proto)?;
        let mut portal = self.portals.remove(&name).ok_or_else(|| Error {
            code: "34000",
            message: format!("portal \"{name}\" does not exist"),
        })?;
        let Some(stmt) = portal.stmt.clone() else {
            self.out
                .write_all(&Msg::new(b'I').finish())
                .map_err(proto)?;
            return Ok(());
        };
        if portal.pending.is_none() {
            let out = self.session.run(&stmt, &portal.params)?;
            let tag = command_tag(&stmt, &out);
            let rows = match out {
                Output::Rows { rows, columns } => {
                    portal.columns = Some(columns);
                    rows.into()
                }
                Output::Command(_) => Default::default(),
            };
            portal.pending = Some((rows, tag));
        }
        let columns = portal.columns.clone().unwrap_or_default();
        let (rows, tag) = portal.pending.as_mut().expect("just set");
        let mut sent = 0;
        while let Some(r) = rows.front() {
            if max > 0 && sent == max {
                break;
            }
            let r = r.clone();
            self.data_row(&r, &columns, &portal.result_formats)
                .map_err(proto)?;
            rows.pop_front();
            sent += 1;
        }
        if rows.is_empty() {
            let tag = tag.clone();
            self.out
                .write_all(&Msg::new(b'C').str(&tag).finish())
                .map_err(proto)?;
            portal.pending = None;
            portal.stmt = None; // a finished portal is not run again
        } else {
            self.out
                .write_all(&Msg::new(b's').finish())
                .map_err(proto)?;
        }
        self.portals.insert(name, portal);
        Ok(())
    }

    fn close(&mut self, body: &[u8]) -> Result<(), Error> {
        let mut b = Body { b: body };
        let kind = b.take(1).map_err(proto)?[0];
        let name = b.cstr().map_err(proto)?;
        if kind == b'S' {
            self.prepared.remove(&name);
        } else {
            self.portals.remove(&name);
        }
        self.out.write_all(&Msg::new(b'3').finish()).map_err(proto)
    }
}

fn format_for(formats: &[i16], i: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(i).copied().unwrap_or(0),
    }
}
