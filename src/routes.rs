use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use crate::{
    auth::{self, Session},
    state::AppState,
    tokens::{self, now, GRACE},
};

// ---------- errors ----------

pub struct ApiError(StatusCode, String);

fn err(code: StatusCode, msg: &str) -> ApiError {
    ApiError(code, msg.to_string())
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "ok": false, "message": self.1 }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!("database error: {e}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Server error. Try again.".into())
    }
}

// ---------- router ----------

pub fn app(st: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/lecturer", get(|| async { Html(include_str!("../static/create.html")) }))
        .route("/lecturer/:id", get(|| async { Html(include_str!("../static/live.html")) }))
        .route("/scan/:id", get(|| async { Html(include_str!("../static/scan.html")) }))
        .route("/a/:token", get(|| async { Html(include_str!("../static/scan.html")) }))
        .route(
            "/app.css",
            get(|| async {
                ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../static/app.css"))
            }),
        )
        .route("/mock/launch", get(mock_launch))
        .route("/api/activities", post(create_activity))
        .route("/api/activities/:id/info", get(info))
        .route("/api/activities/:id/close", post(close_activity))
        .route("/api/activities/:id/attendance", get(attendance_json))
        .route("/api/activities/:id/attendance.csv", get(attendance_csv))
        .route("/api/attend", post(attend))
        .route("/ws/:id", get(ws_handler))
        .with_state(st)
}

// ---------- helpers ----------

#[derive(sqlx::FromRow)]
struct Activity {
    title: String,
    lecturer_id: String,
    open_at: i64,
    close_at: i64,
    closed: i64,
}

impl Activity {
    fn state(&self) -> &'static str {
        let n = now();
        if self.closed != 0 || n >= self.close_at {
            "closed"
        } else if n < self.open_at {
            "scheduled"
        } else {
            "open"
        }
    }
}

async fn load(st: &AppState, id: i64) -> Result<Activity, ApiError> {
    sqlx::query_as::<_, Activity>(
        "SELECT title, lecturer_id, open_at, close_at, closed FROM activities WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Activity not found"))
}

pub async fn activity_state(st: &AppState, id: i64) -> Option<&'static str> {
    load(st, id).await.ok().map(|a| a.state())
}

fn any_session(st: &AppState, h: &HeaderMap) -> Result<Session, ApiError> {
    auth::session_from(h, &st.secret, now())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Please open this from your course page."))
}

fn require(st: &AppState, h: &HeaderMap, role: &str) -> Result<Session, ApiError> {
    let s = any_session(st, h)?;
    if s.role != role {
        return Err(err(StatusCode::FORBIDDEN, "You are not allowed to do that."));
    }
    Ok(s)
}

async fn owned(st: &AppState, h: &HeaderMap, id: i64) -> Result<Activity, ApiError> {
    let s = require(st, h, "lecturer")?;
    let a = load(st, id).await?;
    if a.lecturer_id != s.id {
        return Err(err(StatusCode::FORBIDDEN, "This is not your activity."));
    }
    Ok(a)
}

fn push_cookie(resp: &mut Response, value: String) {
    if let Ok(v) = HeaderValue::from_str(&value) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
}

async fn list_attendance(st: &AppState, id: i64) -> Result<Vec<Value>, sqlx::Error> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT student_id, student_name, marked_at FROM attendance WHERE activity_id = ? ORDER BY marked_at",
    )
    .bind(id)
    .fetch_all(&st.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(i, n, t)| json!({ "student_id": i, "name": n, "marked_at": t }))
        .collect())
}

// ---------- dev-only stand-in for the Moodle LTI launch ----------

#[derive(Deserialize)]
struct MockQ {
    role: String,
    id: String,
    name: String,
    activity: Option<i64>,
}

async fn mock_launch(
    State(st): State<AppState>,
    Query(q): Query<MockQ>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !st.mock_lti {
        return Err(err(StatusCode::NOT_FOUND, "Not found"));
    }
    if q.role != "lecturer" && q.role != "student" {
        return Err(err(StatusCode::BAD_REQUEST, "role must be lecturer or student"));
    }
    let loc = match (q.role.as_str(), q.activity) {
        ("lecturer", Some(a)) => format!("/lecturer/{a}"),
        ("lecturer", None) => "/lecturer".to_string(),
        (_, Some(a)) => format!("/scan/{a}"),
        _ => "/".to_string(),
    };
    let session = Session { role: q.role, id: q.id, name: q.name, exp: now() + 8 * 3600 };
    let secure = st.base_url.starts_with("https");
    let mut resp = Redirect::to(&loc).into_response();
    push_cookie(&mut resp, auth::set_cookie("sid", &auth::sign(&session, &st.secret), 8 * 3600, secure));
    if auth::cookie(&headers, "did").is_none() {
        push_cookie(&mut resp, auth::set_cookie("did", &tokens::new_token(), 365 * 86400, secure));
    }
    Ok(resp)
}

// ---------- lecturer API ----------

#[derive(Deserialize)]
struct NewActivity {
    course_id: String,
    title: String,
    open_at: i64,
    close_at: i64,
}

async fn create_activity(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<NewActivity>,
) -> Result<Json<Value>, ApiError> {
    let s = require(&st, &headers, "lecturer")?;
    if b.title.trim().is_empty() || b.course_id.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Course and title are required."));
    }
    if b.close_at <= b.open_at {
        return Err(err(StatusCode::BAD_REQUEST, "Close time must be after open time."));
    }
    let r = sqlx::query(
        "INSERT INTO activities (course_id, title, lecturer_id, open_at, close_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(b.course_id.trim())
    .bind(b.title.trim())
    .bind(&s.id)
    .bind(b.open_at)
    .bind(b.close_at)
    .execute(&st.db)
    .await?;
    Ok(Json(json!({ "ok": true, "id": r.last_insert_rowid() })))
}

async fn close_activity(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owned(&st, &headers, id).await?;
    sqlx::query("UPDATE activities SET closed = 1 WHERE id = ?").bind(id).execute(&st.db).await?;
    st.clear_current(id);
    st.publish(id, json!({ "type": "closed" }).to_string());
    Ok(Json(json!({ "ok": true })))
}

async fn attendance_json(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owned(&st, &headers, id).await?;
    Ok(Json(json!({ "ok": true, "attendance": list_attendance(&st, id).await? })))
}

const EAT_OFFSET: i64 = 3 * 3600; // Kenya: UTC+3, no daylight saving

/// "YYYY-MM-DD HH:MM:SS EAT" from unix seconds (civil-from-days algorithm).
fn fmt_eat(t: i64) -> String {
    let t = t + EAT_OFFSET;
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} EAT", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Quote a CSV cell and defuse spreadsheet formulas (=, +, -, @).
fn csv_cell(s: &str) -> String {
    let mut v = s.replace('"', "\"\"");
    if matches!(v.chars().next(), Some('=' | '+' | '-' | '@')) {
        v.insert(0, '\'');
    }
    format!("\"{v}\"")
}

async fn attendance_csv(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    owned(&st, &headers, id).await?;
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT student_id, student_name, marked_at FROM attendance WHERE activity_id = ? ORDER BY marked_at",
    )
    .bind(id)
    .fetch_all(&st.db)
    .await?;
    let mut out = String::from("student_id,name,marked_at\n");
    for (sid, name, t) in rows {
        out.push_str(&format!("{},{},{}\n", csv_cell(&sid), csv_cell(&name), csv_cell(&fmt_eat(t))));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"attendance.csv\""),
        ],
        out,
    )
        .into_response())
}

// ---------- shared by both roles ----------

async fn info(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    any_session(&st, &headers)?;
    let a = load(&st, id).await?;
    Ok(Json(json!({ "ok": true, "title": a.title, "state": a.state() })))
}

// ---------- student scan ----------

#[derive(Deserialize)]
struct AttendBody {
    token: String,
}

/// The core of the strict chain. The conditional UPDATE is the atomic step:
/// only one request can ever claim a given code, so two simultaneous scans cannot both win.
async fn attend(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<AttendBody>,
) -> Result<Response, ApiError> {
    let s = require(&st, &headers, "student")?;
    let (device, new_device) = match auth::cookie(&headers, "did") {
        Some(d) => (d, false),
        None => (tokens::new_token(), true),
    };
    let h = tokens::hash(b.token.trim());

    let row: Option<(i64, i64, Option<i64>)> =
        sqlx::query_as("SELECT activity_id, expires_at, used_at FROM tokens WHERE token_hash = ?")
            .bind(&h)
            .fetch_optional(&st.db)
            .await?;
    let (aid, exp, used) = row.ok_or_else(|| {
        err(StatusCode::BAD_REQUEST, "That code is not valid. Scan the QR on the lecturer's screen.")
    })?;

    let a = load(&st, aid).await?;
    match a.state() {
        "scheduled" => return Err(err(StatusCode::FORBIDDEN, "Attendance has not opened yet.")),
        "closed" => return Err(err(StatusCode::FORBIDDEN, "Attendance is closed.")),
        _ => {}
    }
    if used.is_some() || exp + GRACE < now() {
        return Err(err(StatusCode::GONE, "That code has expired. Scan the new one on the screen."));
    }

    let already: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM attendance WHERE activity_id = ? AND student_id = ?")
            .bind(aid)
            .bind(&s.id)
            .fetch_optional(&st.db)
            .await?;
    if already.is_some() {
        return Ok(Json(json!({ "ok": true, "message": "You are already marked present." })).into_response());
    }
    let other: Option<(String,)> =
        sqlx::query_as("SELECT student_id FROM attendance WHERE activity_id = ? AND device_id = ?")
            .bind(aid)
            .bind(&device)
            .fetch_optional(&st.db)
            .await?;
    if other.is_some() {
        return Err(err(StatusCode::CONFLICT, "This device has already marked another student."));
    }

    // Claim the code. Zero rows changed means someone else got there first.
    let t = now();
    let claimed = sqlx::query("UPDATE tokens SET used_by = ?, used_at = ? WHERE token_hash = ? AND used_at IS NULL")
        .bind(&s.id)
        .bind(t)
        .bind(&h)
        .execute(&st.db)
        .await?
        .rows_affected();
    if claimed == 0 {
        return Err(err(StatusCode::GONE, "Someone just used that code. Scan the new one."));
    }

    let ins = sqlx::query(
        "INSERT INTO attendance (activity_id, student_id, student_name, device_id, token_hash, marked_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(aid)
    .bind(&s.id)
    .bind(&s.name)
    .bind(&device)
    .bind(&h)
    .bind(t)
    .execute(&st.db)
    .await;

    if ins.is_ok() {
        st.publish(
            aid,
            json!({ "type": "marked", "student_id": s.id, "name": s.name, "marked_at": t }).to_string(),
        );
    }
    // The code is spent either way, so the screen must move on to a new one.
    if let Err(e) = tokens::rotate(&st, aid).await {
        tracing::error!("rotate failed: {e}");
    }
    if let Err(e) = ins {
        tracing::warn!("attendance insert failed: {e}");
        return Err(err(StatusCode::CONFLICT, "Could not record attendance. You may already be marked."));
    }

    let mut resp = Json(json!({ "ok": true, "message": "Attendance recorded." })).into_response();
    if new_device {
        let secure = st.base_url.starts_with("https");
        push_cookie(&mut resp, auth::set_cookie("did", &device, 365 * 86400, secure));
    }
    Ok(resp)
}

// ---------- lecturer live screen (WebSocket) ----------

async fn ws_handler(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    owned(&st, &headers, id).await?;
    Ok(ws.on_upgrade(move |socket| live_socket(st, id, socket)))
}

async fn send_snapshot(st: &AppState, id: i64, socket: &mut WebSocket) -> Result<(), axum::Error> {
    let Ok(a) = load(st, id).await else { return Ok(()) };
    let list = list_attendance(st, id).await.unwrap_or_default();
    let init = json!({ "type": "init", "title": a.title, "state": a.state(), "attendance": list });
    socket.send(Message::Text(init.to_string())).await?;
    if a.state() == "open" {
        if let Ok((t, e)) = tokens::ensure_current(st, id).await {
            socket.send(Message::Text(tokens::qr_message(st, &t, e))).await?;
        }
    }
    Ok(())
}

async fn live_socket(st: AppState, id: i64, mut socket: WebSocket) {
    // Subscribe first so no event can slip between the snapshot and the live feed.
    let mut rx = st.subscribe(id);
    if send_snapshot(&st, id, &mut socket).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Ok(text) => {
                    if socket.send(Message::Text(text)).await.is_err() { break; }
                }
                Err(RecvError::Lagged(_)) => {
                    if send_snapshot(&st, id, &mut socket).await.is_err() { break; }
                }
                Err(RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eat_formatting() {
        assert_eq!(fmt_eat(0), "1970-01-01 03:00:00 EAT");
        assert_eq!(fmt_eat(1_794_000_000), "2026-11-07 00:20:00 EAT");
    }

    #[test]
    fn csv_defuses_formulas() {
        assert_eq!(csv_cell("=1+1"), "\"'=1+1\"");
        assert_eq!(csv_cell("a\"b"), "\"a\"\"b\"");
    }
}
