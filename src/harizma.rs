use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, Utc};

use crate::slots::AvailableSlot;

const BASE_URL: &str = "https://harizma45.ru";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// The clinic is a single office; slots are for the online-booking flow.
const OFFICE_NAME: &str = "МЦ «Харизма», г. Курган, ул. Куйбышева, 27";
const SERVICE_NAME: &str = "Онлайн-запись";

/// Minimal client for the harizma45.ru public booking pages.
///
/// Unlike Invitro there is no JSON API: the only live availability signal is
/// the server-rendered "ближайший прием: <date>" of the doctor's `.item`
/// card on a `/register/...` page. This client fetches and parses those pages.
#[derive(Clone)]
pub struct HarizmaClient {
    http: reqwest::Client,
    base_url: String,
}

/// A doctor card as rendered in the carousel on a `/register/` page.
#[derive(Debug, Clone)]
pub struct DoctorItem {
    /// Internal schedule id (`input[name="doctor_id"]`, `doc_<id>`).
    pub doctor_id: u64,
    pub full_name: String,
    /// Job/specialty line, e.g. "врач УЗИ-диагностики".
    pub job: String,
    /// Booking page path, e.g. `/register/uzi-diagnostika/vozhdaeva-anastasiya/`.
    pub page_path: String,
    /// "ближайший прием" date as rendered by the site (e.g. `19 октября`),
    /// or `None` when the doctor currently has no online appointments.
    pub next_appointment: Option<String>,
}

/// Everything needed to poll one harizma doctor, resolved once per cycle.
#[derive(Debug, Clone)]
pub struct HarizmaTarget {
    pub doctor_id: u64,
    pub doctor_name: String,
    pub job: String,
    pub page_path: String,
}

impl HarizmaTarget {
    pub fn from_item(item: &DoctorItem) -> Self {
        HarizmaTarget {
            doctor_id: item.doctor_id,
            doctor_name: item.full_name.clone(),
            job: item.job.clone(),
            page_path: item.page_path.clone(),
        }
    }

    /// Booking page URL, e.g.
    /// `https://harizma45.ru/register/uzi-diagnostika/vozhdaeva-anastasiya/`.
    pub fn booking_link(&self) -> String {
        format!("{BASE_URL}{}", self.page_path)
    }
}

impl HarizmaClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            base_url: BASE_URL.to_string(),
        })
    }

    /// A client for tests: it talks to a local mock server and ignores proxies.
    #[cfg(test)]
    pub fn with_base_url(base_url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .no_proxy()
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.to_string(),
        })
    }

    async fn get_html(&self, path: &str) -> Result<String> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("request failed: {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{url} -> HTTP {status}: {}", truncate(&body, 300));
        }
        Ok(body)
    }

    /// Parse the `#owlReg` carousel of the "Запись на прием" page, which lists
    /// every doctor (across all specialties) together with their current
    /// "ближайший прием" dates. One request, no per-doctor round trips.
    pub async fn list_doctors(&self) -> Result<Vec<DoctorItem>> {
        let html = self.get_html("/register/").await?;
        let items = parse_items(&html);
        if items.is_empty() {
            anyhow::bail!("no doctor cards found on {BASE_URL}/register/");
        }
        Ok(items)
    }

    /// Fetch one doctor's booking page and return its card. Fails when the
    /// expected doctor is not on the page (page moved / wrong id).
    pub async fn doctor_on_page(&self, page_path: &str, doctor_id: u64) -> Result<DoctorItem> {
        let html = self.get_html(page_path).await?;
        let items = parse_items(&html);
        let found = items.iter().find(|d| d.doctor_id == doctor_id).cloned();
        found.with_context(|| {
            format!(
                "doctor id {doctor_id} not found on {BASE_URL}{page_path} ({} doctor(s) parsed)",
                items.len()
            )
        })
    }

    /// Pick a single doctor out of search results for a name query.
    ///
    /// The list is filtered to candidates whose name contains the query first,
    /// then an exact (normalized) full-name match is preferred; otherwise a
    /// unique candidate is accepted. Ambiguous queries are rejected with the
    /// candidate list.
    pub fn pick_doctor<'a>(doctors: &'a [DoctorItem], query: &str) -> Result<&'a DoctorItem> {
        let norm = normalize_name(query);
        let candidates: Vec<&DoctorItem> = doctors
            .iter()
            .filter(|d| normalize_name(&d.full_name).contains(&norm))
            .collect();
        if candidates.is_empty() {
            anyhow::bail!("no doctor found by name '{query}' on harizma45.ru");
        }
        let exact: Vec<&DoctorItem> = candidates
            .iter()
            .copied()
            .filter(|d| normalize_name(&d.full_name) == norm)
            .collect();
        if exact.len() == 1 {
            return Ok(exact[0]);
        }
        if candidates.len() == 1 {
            return Ok(candidates[0]);
        }
        let list = candidates
            .iter()
            .map(|d| format!("  - {} (id {})", d.full_name, d.doctor_id))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!("ambiguous doctor name '{query}', matches:\n{list}")
    }

    /// Resolve the watch target: by exact name (searches the register page)
    /// or directly by id + page path.
    pub async fn resolve_target(
        &self,
        doctor_id: u64,
        page_path: &str,
        doctor_name: Option<&str>,
    ) -> Result<HarizmaTarget> {
        match doctor_name {
            Some(name) => {
                let doctors = self.list_doctors().await?;
                let item = Self::pick_doctor(&doctors, name)?;
                Ok(HarizmaTarget::from_item(item))
            }
            None => {
                let item = self.doctor_on_page(page_path, doctor_id).await?;
                Ok(HarizmaTarget::from_item(&item))
            }
        }
    }

    /// Fetch the currently available slot(s) for the target.
    ///
    /// harizma45.ru exposes only the doctor's next appointment date (no exact
    /// times, no 30-day calendar), so the result is 0 or 1 slot whose `date`
    /// is the normalized next-appointment date and `time` is empty.
    pub async fn fetch_available_slots(
        &self,
        target: &HarizmaTarget,
        today: &str,
    ) -> Result<Vec<AvailableSlot>> {
        let item = self
            .doctor_on_page(&target.page_path, target.doctor_id)
            .await?;
        let Some(raw) = item.next_appointment else {
            return Ok(Vec::new());
        };
        let date = parse_ru_date(&raw, today).unwrap_or(raw);
        Ok(vec![AvailableSlot {
            office_id: format!("harizma-{}", target.doctor_id),
            office_name: OFFICE_NAME.to_string(),
            service_id: "online".to_string(),
            service_name: SERVICE_NAME.to_string(),
            price: String::new(),
            date,
            time: String::new(),
        }])
    }
}

// ---------- HTML parsing ----------

/// Split the carousel HTML into per-doctor blocks and parse each.
///
/// Blocks are delimited by `<div class="item" id="doc_` markers (each card is
/// a sibling inside `#owlReg`); the last card runs to the end of the page.
fn parse_items(html: &str) -> Vec<DoctorItem> {
    const MARKER: &str = r#"class="item" id="doc_"#;
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = html[cursor..].find(MARKER) {
        let start = cursor + rel;
        // Search for the *next* card after the current marker; the last card
        // runs to the end of the page.
        let end = match html[start + MARKER.len()..].find(MARKER) {
            Some(next) => start + MARKER.len() + next,
            None => html.len(),
        };
        if let Some(item) = parse_item(&html[start..end]) {
            out.push(item);
        }
        cursor = end;
    }
    out
}

fn parse_item(block: &str) -> Option<DoctorItem> {
    let doctor_id = find_attr_u64(block, r#"name="doctor_id" value=""#)?;
    let full_name = find_div_text(block, r#"<div class="name">"#)?;
    let job = find_text_before(block, r#"<div class="job">"#, "<br/>")?;
    let page_path = find_quoted(block, "document.location.href = '")?;
    let page_path = page_path
        .split('?')
        .next()
        .unwrap_or(&page_path)
        .to_string();
    let next_appointment = find_next_appointment(block);
    Some(DoctorItem {
        doctor_id,
        full_name,
        job,
        page_path,
        next_appointment,
    })
}

/// Read an attribute value after `prefix`, up to the next `"`.
fn find_attr_u64(s: &str, prefix: &str) -> Option<u64> {
    let start = s.find(prefix)? + prefix.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    rest[..end].trim().parse().ok()
}

/// Plain text of the first `<div class="...">` matching `marker`.
fn find_div_text(s: &str, marker: &str) -> Option<String> {
    let start = s.find(marker)? + marker.len();
    let rest = &s[start..];
    let end = rest.find("</div>")?;
    let text = rest[..end].trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Text between `marker` and the next `terminator` (both exclusive), trimmed.
fn find_text_before(s: &str, marker: &str, terminator: &str) -> Option<String> {
    let start = s.find(marker)? + marker.len();
    let rest = &s[start..];
    let end = rest.find(terminator)?;
    let text = rest[..end].trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Text between the opening quote after `marker` and the closing `'`.
fn find_quoted(s: &str, marker: &str) -> Option<String> {
    let start = s.find(marker)? + marker.len();
    let rest = &s[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Extract the date after `ближайший прием:<br/>` inside `.time`, e.g. "19 октября".
fn find_next_appointment(block: &str) -> Option<String> {
    const MARKER: &str = "ближайший прием:";
    let start = block.find(MARKER)?;
    let after = &block[start + MARKER.len()..];
    let br = after.find("<br/>")?;
    let after_br = &after[br + 5..];
    let end = after_br.find("</div>")?;
    let text = after_br[..end].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Parse a Russian "D MMMM" date (e.g. `19 октября`) into `YYYY-MM-DD`.
///
/// The site never renders the year, so the current year is assumed; if the
/// result lies before `today` the next year is used instead (a site can show
/// a date that is "near" a year boundary).
fn parse_ru_date(raw: &str, today: &str) -> Option<String> {
    let mut parts = raw.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year = Utc::now().year();
    let mut date = NaiveDate::from_ymd_opt(year, month, day)?;
    if date.to_string().as_str() < today {
        date = NaiveDate::from_ymd_opt(year + 1, month, day)?;
    }
    Some(date.format("%Y-%m-%d").to_string())
}

fn month_number(name: &str) -> Option<u32> {
    Some(match name.trim().trim_end_matches('.') {
        "январь" | "января" => 1,
        "февраль" | "февраля" => 2,
        "март" | "марта" => 3,
        "апрель" | "апреля" => 4,
        "май" | "мая" => 5,
        "июнь" | "июня" => 6,
        "июль" | "июля" => 7,
        "август" | "августа" => 8,
        "сентябрь" | "сентября" => 9,
        "октябрь" | "октября" => 10,
        "ноябрь" | "ноября" => 11,
        "декабрь" | "декабря" => 12,
        _ => return None,
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Normalize a name for comparison: lowercase and collapse whitespace.
fn normalize_name(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{MockServer, Route};

    const SAMPLE: &str = r#"
    <div id="owlReg" class="owl-carousel owl-theme">
        <div class="item" id="doc_303"
             onclick="document.location.href = '/register/uzi-diagnostika/stolnikova-elena/?DOCTOR_ID=5890'">
            <input type="hidden" name="doctor_id" value="303"/>
            <input type="hidden" name="doctor_wait" value="0"/>
            <div class="photo" style="background:url(/x.jpg) center no-repeat;"></div>
            <div class="name">Стольникова Елена Сергеевна </div>
            <div class="job">врач УЗИ-диагностики<br/>
                <div class="time">ближайший прием:<br/>
                    19 октября</div>
            </div>
            <div class="action"><span class="but">Выбрать</span></div>
            <script>doctor[303] = {};</script>
        </div>
        <div class="item" id="doc_940"
             onclick="document.location.href = '/register/uzi-diagnostika/vozhdaeva-anastasiya/?DOCTOR_ID=5893'">
            <input type="hidden" name="doctor_id" value="940"/>
            <input type="hidden" name="doctor_wait" value="0"/>
            <div class="photo" style="background:url(/y.jpg) center no-repeat;"></div>
            <div class="name">Вождаева Анастасия Викторовна </div>
            <div class="job">врач УЗИ-диагностики<br/>
            </div>
            <div class="action"><span class="but">Выбрать</span></div>
            <script>doctor[940] = {};</script>
        </div>
    </div>
    "#;

    #[test]
    fn parses_cards_with_and_without_next_appointment() {
        let items = parse_items(SAMPLE);
        assert_eq!(items.len(), 2);

        let stol = &items[0];
        assert_eq!(stol.doctor_id, 303);
        assert_eq!(stol.full_name, "Стольникова Елена Сергеевна");
        assert_eq!(stol.job, "врач УЗИ-диагностики");
        assert_eq!(
            stol.page_path,
            "/register/uzi-diagnostika/stolnikova-elena/"
        );
        assert_eq!(stol.next_appointment.as_deref(), Some("19 октября"));

        let vozh = &items[1];
        assert_eq!(vozh.doctor_id, 940);
        assert_eq!(vozh.full_name, "Вождаева Анастасия Викторовна");
        assert_eq!(
            vozh.page_path,
            "/register/uzi-diagnostika/vozhdaeva-anastasiya/"
        );
        assert_eq!(vozh.next_appointment, None);
    }

    #[test]
    fn parses_real_page_snippet() {
        // A slice of the actual /register/ page around Вождаева's card.
        let html = r#"
        <div class="item" id="doc_886213"
             onclick="document.location.href = '/register/uzi-diagnostika/morozova-anastasiya-andreevna/?DOCTOR_ID=886220'">
            <input type="hidden" name="doctor_id" value="886213"/>
            <input type="hidden" name="doctor_wait" value="0"/>
            <div class="name">Морозова Анастасия Андреевна </div>
            <div class="job">врач УЗИ-диагностики<br/>
            </div>
            <div class="action"><span class="but">Выбрать</span></div>
            <script>doctor[886213] = {};</script>
        </div>
        <div class="item" id="doc_940"
             onclick="document.location.href = '/register/uzi-diagnostika/vozhdaeva-anastasiya/?DOCTOR_ID=5893'">
            <input type="hidden" name="doctor_id" value="940"/>
            <input type="hidden" name="doctor_wait" value="0"/>
            <div class="name">Вождаева Анастасия Викторовна </div>
            <div class="job">врач УЗИ-диагностики<br/>
            </div>
            <div class="action"><span class="but">Выбрать</span></div>
            <script>doctor[940] = {};</script>
        </div>
        "#;
        let items = parse_items(html);
        let vozh = items.iter().find(|d| d.doctor_id == 940).unwrap();
        assert_eq!(vozh.full_name, "Вождаева Анастасия Викторовна");
        assert_eq!(vozh.next_appointment, None);
    }

    #[test]
    fn pick_prefers_exact_full_name_match() {
        let items = parse_items(SAMPLE);
        let picked = HarizmaClient::pick_doctor(&items, "вождаева анастасия викторовна").unwrap();
        assert_eq!(picked.doctor_id, 940);
    }

    #[test]
    fn pick_accepts_unique_substring_match() {
        let items = parse_items(SAMPLE);
        let only_vozh: Vec<DoctorItem> = items
            .iter()
            .filter(|d| d.doctor_id == 940)
            .cloned()
            .collect();
        let picked = HarizmaClient::pick_doctor(&only_vozh, "Вождаева").unwrap();
        assert_eq!(picked.doctor_id, 940);
    }

    #[test]
    fn pick_rejects_ambiguous_matches() {
        let html = r#"
        <div class="item" id="doc_1"
             onclick="document.location.href = '/register/a/dok1/?DOCTOR_ID=1'">
            <input type="hidden" name="doctor_id" value="1"/>
            <div class="name">Анастасия Ивановна Первая </div>
            <div class="job">врач<br/>
            </div>
        </div>
        <div class="item" id="doc_2"
             onclick="document.location.href = '/register/b/dok2/?DOCTOR_ID=2'">
            <input type="hidden" name="doctor_id" value="2"/>
            <div class="name">Анастасия Петровна Вторая </div>
            <div class="job">врач<br/>
            </div>
        </div>
        "#;
        let items = parse_items(html);
        let err = HarizmaClient::pick_doctor(&items, "Анастасия")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("Первая"));
        assert!(err.contains("Вторая"));
    }

    #[test]
    fn booking_link_uses_page_path() {
        let target = HarizmaTarget {
            doctor_id: 940,
            doctor_name: "Вождаева Анастасия Викторовна".into(),
            job: "врач УЗИ-диагностики".into(),
            page_path: "/register/uzi-diagnostika/vozhdaeva-anastasiya/".into(),
        };
        assert_eq!(
            target.booking_link(),
            "https://harizma45.ru/register/uzi-diagnostika/vozhdaeva-anastasiya/"
        );
    }

    #[test]
    fn ru_date_parses_to_iso() {
        assert_eq!(
            parse_ru_date("19 октября", "2026-10-19").as_deref(),
            Some("2026-10-19")
        );
        assert_eq!(
            parse_ru_date("3 марта", "2026-10-19").as_deref(),
            Some("2027-03-03")
        );
        assert_eq!(
            parse_ru_date("1 января", "2026-10-19").as_deref(),
            Some("2027-01-01")
        );
        assert!(parse_ru_date("не дата", "2026-10-19").is_none());
    }

    #[tokio::test]
    async fn list_doctors_fetches_register_page() {
        let server = MockServer::start(vec![Route::html("/register/", SAMPLE)]).await;
        let client = HarizmaClient::with_base_url(server.base_url()).unwrap();

        let doctors = client.list_doctors().await.unwrap();

        assert_eq!(doctors.len(), 2);
        assert_eq!(doctors[0].full_name, "Стольникова Елена Сергеевна");
        assert_eq!(doctors[1].full_name, "Вождаева Анастасия Викторовна");
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.requests()[0].target, "/register/");
    }

    #[tokio::test]
    async fn list_doctors_rejects_page_without_cards() {
        let server = MockServer::start(vec![Route::html("/register/", "<html></html>")]).await;
        let client = HarizmaClient::with_base_url(server.base_url()).unwrap();

        let err = client.list_doctors().await.unwrap_err().to_string();

        assert!(err.contains("no doctor cards found"));
    }

    #[tokio::test]
    async fn resolve_target_by_name_searches_register_page() {
        let server = MockServer::start(vec![Route::html("/register/", SAMPLE)]).await;
        let client = HarizmaClient::with_base_url(server.base_url()).unwrap();

        let target = client
            .resolve_target(0, "", Some("Вождаева"))
            .await
            .unwrap();

        assert_eq!(target.doctor_id, 940);
        assert_eq!(target.doctor_name, "Вождаева Анастасия Викторовна");
        assert_eq!(target.job, "врач УЗИ-диагностики");
        assert_eq!(
            target.page_path,
            "/register/uzi-diagnostika/vozhdaeva-anastasiya/"
        );
    }

    #[tokio::test]
    async fn doctor_on_page_finds_doctor_by_id() {
        let path = "/register/uzi-diagnostika/vozhdaeva-anastasiya/";
        let server = MockServer::start(vec![Route::html(path, SAMPLE)]).await;
        let client = HarizmaClient::with_base_url(server.base_url()).unwrap();

        let item = client.doctor_on_page(path, 940).await.unwrap();

        assert_eq!(item.full_name, "Вождаева Анастасия Викторовна");
        assert_eq!(item.next_appointment, None);
    }

    #[tokio::test]
    async fn doctor_on_page_rejects_unknown_id() {
        let path = "/register/uzi-diagnostika/vozhdaeva-anastasiya/";
        let server = MockServer::start(vec![Route::html(path, SAMPLE)]).await;
        let client = HarizmaClient::with_base_url(server.base_url()).unwrap();

        let err = client
            .doctor_on_page(path, 999)
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("doctor id 999 not found"));
    }
}
