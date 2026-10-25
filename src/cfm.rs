use anyhow::{Context, Result};
use chrono::NaiveDate;

use crate::config::CfmWatch;
use crate::slots::AvailableSlot;

const BASE_URL: &str = "https://www.cfm.ru";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// The watched clinic: the Курган branch of the cfm.ru network.
const CITY: &str = "kurgan";
/// The clinic is a single office; slots are for the online-booking flow.
const OFFICE_NAME: &str = "ЦСМ, г. Курган, ул. Карбышева, 39";

/// Minimal client for the cfm.ru public booking flow.
///
/// Unlike Invitro there is no JSON API, but the flow is server-rendered:
/// `?step=1` lists the directions (services) of the city,
/// `?step=2&service=…&clinic=0` lists the doctors of one service,
/// `?step=2&…&doctor=…` embeds the doctor's free dates as a JS array
/// (`freeDates`), and `?step=2&…&doctor=…&date=…` lists the free times
/// of one date as `step=3` links.
///
/// The theme answers these URLs with its 404 page unless the request looks
/// like the site's own booking iframe, so the client always sends
/// `Sec-Fetch-Dest: iframe` (plus the matching mode and site headers).
#[derive(Clone)]
pub struct CfmClient {
    http: reqwest::Client,
    base_url: String,
}

/// A service (direction) from the first booking step.
#[derive(Debug, Clone)]
pub struct ServiceItem {
    pub service_id: u64,
    pub name: String,
}

/// A doctor card as rendered in the second booking step.
#[derive(Debug, Clone)]
pub struct DoctorItem {
    pub service_id: u64,
    pub doctor_id: u64,
    pub full_name: String,
    /// Job/specialty line, e.g. "Врач-уролог".
    pub job: String,
    /// Price line without the ₽ sign, e.g. "от 2500" (may be empty).
    pub price: String,
}

/// A doctor's booking page: the doctor's card and the free dates.
#[derive(Debug, Clone)]
pub struct DoctorPage {
    pub doctor: DoctorItem,
    /// Free dates in `YYYY-MM-DD`, in the order the site lists them.
    pub free_dates: Vec<String>,
}

/// Everything needed to poll one cfm.ru doctor, resolved once per cycle.
#[derive(Debug, Clone)]
pub struct CfmTarget {
    pub service_id: u64,
    pub service_name: String,
    pub doctor_id: u64,
    pub doctor_name: String,
    pub job: String,
    pub price: String,
}

impl CfmTarget {
    pub fn from_item(item: &DoctorItem, service_name: &str) -> Self {
        CfmTarget {
            service_id: item.service_id,
            service_name: service_name.to_string(),
            doctor_id: item.doctor_id,
            doctor_name: item.full_name.clone(),
            job: item.job.clone(),
            price: item.price.clone(),
        }
    }

    /// Booking page URL, e.g.
    /// `https://www.cfm.ru/kurgan/zapis/?step=2&service=3&clinic=0&doctor=6164`.
    pub fn booking_link(&self) -> String {
        format!(
            "{BASE_URL}/{CITY}/zapis/?step=2&service={}&clinic=0&doctor={}",
            self.service_id, self.doctor_id
        )
    }
}

impl CfmClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(iframe_headers())
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            base_url: format!("{BASE_URL}/{CITY}"),
        })
    }

    /// A client for tests: it talks to a local mock server and ignores proxies.
    #[cfg(test)]
    pub fn with_base_url(base_url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .no_proxy()
            .default_headers(iframe_headers())
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

    /// The directions (services) of the city from the first booking step.
    pub async fn list_services(&self) -> Result<Vec<ServiceItem>> {
        let html = self.get_html("/zapis/?step=1").await?;
        let services = parse_services(&html);
        if services.is_empty() {
            anyhow::bail!("no services found on {BASE_URL}/{CITY}/zapis/");
        }
        Ok(services)
    }

    /// The doctors of one service from the second booking step.
    pub async fn list_doctors(&self, service_id: u64) -> Result<Vec<DoctorItem>> {
        let html = self
            .get_html(&format!("/zapis/?step=2&service={service_id}&clinic=0"))
            .await?;
        let doctors = parse_doctors(&html, service_id, None);
        if doctors.is_empty() {
            anyhow::bail!(
                "no doctor cards found for service {service_id} on {BASE_URL}/{CITY}/zapis/"
            );
        }
        Ok(doctors)
    }

    /// Fetch one doctor's booking page: the card and the free dates. Fails when
    /// the expected doctor is not on the page (page moved / wrong id).
    pub async fn doctor_page(&self, service_id: u64, doctor_id: u64) -> Result<DoctorPage> {
        let html = self
            .get_html(&format!(
                "/zapis/?step=2&service={service_id}&clinic=0&doctor={doctor_id}"
            ))
            .await?;
        let doctors = parse_doctors(&html, service_id, Some(doctor_id));
        let doctor = doctors
            .into_iter()
            .find(|d| d.doctor_id == doctor_id)
            .with_context(|| {
                format!(
                    "doctor id {doctor_id} not found on {BASE_URL}/{CITY}/zapis/ \
                     (service {service_id})"
                )
            })?;
        Ok(DoctorPage {
            doctor,
            free_dates: parse_free_dates(&html),
        })
    }

    /// The free times of one date of the doctor's booking page.
    pub async fn slots_on_date(
        &self,
        service_id: u64,
        doctor_id: u64,
        date: &str,
    ) -> Result<Vec<String>> {
        let html = self
            .get_html(&format!(
                "/zapis/?step=2&service={service_id}&clinic=0&doctor={doctor_id}&date={date}"
            ))
            .await?;
        Ok(parse_slot_times(&html))
    }

    /// Pick a single doctor out of the cards for a name query.
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
            anyhow::bail!("no doctor found by name '{query}' on cfm.ru");
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

    /// Resolve the watch targets from the configured list: each entry is a
    /// service id plus a doctor id or name.
    ///
    /// The services are fetched once; the service of every entry must exist.
    /// A doctor given by name is searched among the doctors of its service
    /// (exact match preferred; ambiguous names are rejected).
    pub async fn resolve_targets(&self, watches: &[CfmWatch]) -> Result<Vec<CfmTarget>> {
        let services = self.list_services().await?;
        let mut targets = Vec::new();
        for watch in watches {
            let service_name = services
                .iter()
                .find(|s| s.service_id == watch.service_id)
                .map(|s| s.name.clone())
                .with_context(|| {
                    format!(
                        "service {} not found on {BASE_URL}/{CITY}/zapis/ (check CFM_DOCTORS)",
                        watch.service_id
                    )
                })?;
            let item = match watch.doctor_id() {
                Some(doctor_id) => self.doctor_page(watch.service_id, doctor_id).await?.doctor,
                None => {
                    let doctors = self.list_doctors(watch.service_id).await?;
                    Self::pick_doctor(&doctors, &watch.doctor)?.clone()
                }
            };
            targets.push(CfmTarget::from_item(&item, &service_name));
        }
        Ok(targets)
    }

    /// Fetch the currently available slots for the target: for every free date of
    /// the doctor, the free times of that date.
    pub async fn fetch_available_slots(&self, target: &CfmTarget) -> Result<Vec<AvailableSlot>> {
        let page = self
            .doctor_page(target.service_id, target.doctor_id)
            .await?;
        let mut slots = Vec::new();
        for date in &page.free_dates {
            let times = self
                .slots_on_date(target.service_id, target.doctor_id, date)
                .await?;
            for time in times {
                slots.push(AvailableSlot {
                    office_id: format!("cfm-{CITY}-{}", target.doctor_id),
                    office_name: OFFICE_NAME.to_string(),
                    service_id: target.service_id.to_string(),
                    service_name: target.service_name.clone(),
                    price: target.price.clone(),
                    date: date.clone(),
                    time,
                });
            }
        }
        Ok(slots)
    }
}

/// The booking pages only render for the site's own booking iframe.
fn iframe_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "sec-fetch-dest",
        reqwest::header::HeaderValue::from_static("iframe"),
    );
    headers.insert(
        "sec-fetch-mode",
        reqwest::header::HeaderValue::from_static("navigate"),
    );
    headers.insert(
        "sec-fetch-site",
        reqwest::header::HeaderValue::from_static("same-origin"),
    );
    headers
}

// ---------- HTML parsing ----------

/// Service links on the first step, e.g.
/// `<a href="…/zapis/?step=2&service=3&clinic=0"><span …>Акушеры-гинекологи</span>…`.
fn parse_services(html: &str) -> Vec<ServiceItem> {
    let mut out = Vec::new();
    for (href, text) in find_anchors(html) {
        if query_param(&href, "step").as_deref() != Some("2")
            || query_param(&href, "doctor").is_some()
        {
            continue;
        }
        let Some(service_id) = query_param(&href, "service").and_then(|v| v.parse().ok()) else {
            continue;
        };
        if text.is_empty() || out.iter().any(|s: &ServiceItem| s.service_id == service_id) {
            continue;
        }
        out.push(ServiceItem {
            service_id,
            name: text,
        });
    }
    out
}

/// Doctor cards on the second step, e.g.
/// `<div class="doc-item …"><a href="…&doctor=6164" …></a>…<div class="title f-16 mb-4 line-2">…`.
///
/// `fallback_doctor_id` is used for the card of an already selected doctor,
/// which has no link of its own.
fn parse_doctors(html: &str, service_id: u64, fallback_doctor_id: Option<u64>) -> Vec<DoctorItem> {
    const MARKER: &str = "\"doc-item";
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
        if let Some(item) = parse_doctor(&html[start..end], service_id, fallback_doctor_id) {
            out.push(item);
        }
        cursor = end;
    }
    out
}

fn parse_doctor(
    block: &str,
    service_id: u64,
    fallback_doctor_id: Option<u64>,
) -> Option<DoctorItem> {
    let doctor_id = find_anchors(block)
        .into_iter()
        .find_map(|(href, _)| query_param(&href, "doctor"))
        .and_then(|v| v.parse().ok())
        .or(fallback_doctor_id)?;
    let full_name = find_marked_text(block, "title f-16 mb-4 line-2")?;
    let job = find_marked_text(block, "f-14 cgray lh-13 line-1")?;
    let price = parse_price(block);
    Some(DoctorItem {
        service_id,
        doctor_id,
        full_name,
        job,
        price,
    })
}

/// Price line of a doctor card: the second `ml-8` span after the info row
/// (the first one is the office address), e.g. `от 2500 ₽` or
/// `Первичный 2500₽<br>Повторный 2000 ₽`. The site's ₽ sign is dropped
/// because the notifier adds its own.
fn parse_price(block: &str) -> String {
    let Some(row) = block.find("class = \"row-5") else {
        return String::new();
    };
    let spans = find_class_spans(&block[row..], "ml-8");
    let raw = spans.get(1).cloned().unwrap_or_default();
    raw.replace('₽', "").trim().to_string()
}

/// Free dates of the doctor from the `freeDates` JS array of the booking
/// page (`var tek_data2 = '25.11.2026'; var freeDates = ["25.11.2026", …]`),
/// normalized to `YYYY-MM-DD`.
fn parse_free_dates(html: &str) -> Vec<String> {
    let Some(start) = html.find("freeDates") else {
        return Vec::new();
    };
    let rest = &html[start..];
    let Some(eq) = rest.find('=') else {
        return Vec::new();
    };
    let rest = &rest[eq + 1..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let rest = &rest[open..];
    let Some(close) = rest.find(']') else {
        return Vec::new();
    };
    let dates: Vec<String> = serde_json::from_str(&rest[..=close]).unwrap_or_default();
    dates.iter().filter_map(|d| ru_date_to_iso(d)).collect()
}

/// Parse a Russian `DD.MM.YYYY` date into `YYYY-MM-DD`.
fn ru_date_to_iso(raw: &str) -> Option<String> {
    let date = NaiveDate::parse_from_str(raw.trim(), "%d.%m.%Y").ok()?;
    Some(date.format("%Y-%m-%d").to_string())
}

/// Free times of one date: the `step=3` links of the slot block, e.g.
/// `…/zapis/?step=3&…&date=2026-11-11&time=12:00`.
fn parse_slot_times(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (href, _) in find_anchors(html) {
        if query_param(&href, "step").as_deref() != Some("3") {
            continue;
        }
        if let Some(time) = query_param(&href, "time") {
            if !time.is_empty() {
                out.push(time);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// All `<a …>…</a>` anchors of a page: `(href, text)` pairs.
fn find_anchors(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = html[cursor..].find("<a") {
        let start = cursor + rel;
        let after = &html[start + 2..];
        // Skip tags such as `<article` that merely start with `<a`.
        if !after.starts_with(|c: char| c.is_whitespace() || c == '>') {
            cursor = start + 2;
            continue;
        }
        let Some(gt) = html[start..].find('>') else {
            break;
        };
        let tag_end = start + gt;
        let Some(end_rel) = html[tag_end..].find("</a>") else {
            break;
        };
        let href = find_attr(&html[start..tag_end], "href").unwrap_or_default();
        let text = strip_tags(&html[tag_end + 1..tag_end + end_rel]);
        out.push((href, text));
        cursor = tag_end + end_rel + "</a>".len();
    }
    out
}

/// Text of every `<span>` whose class attribute contains `class_name`.
fn find_class_spans(s: &str, class_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = s[cursor..].find("<span") {
        let start = cursor + rel;
        let Some(gt) = s[start..].find('>') else {
            break;
        };
        let tag_end = start + gt;
        let Some(end_rel) = s[tag_end..].find("</span>") else {
            break;
        };
        let class = find_attr(&s[start..tag_end], "class").unwrap_or_default();
        if class.split_whitespace().any(|c| c == class_name) {
            out.push(strip_tags(&s[tag_end + 1..tag_end + end_rel]));
        }
        cursor = tag_end + end_rel + "</span>".len();
    }
    out
}

/// Text of the first element whose attributes contain `marker` (a class fragment
/// such as `title f-16 mb-4 line-2`), up to the closing tag.
fn find_marked_text(s: &str, marker: &str) -> Option<String> {
    let start = s.find(marker)?;
    let after = &s[start + marker.len()..];
    let gt = after.find('>')?;
    let rest = &after[gt + 1..];
    let end = rest.find("</div>")?;
    let text = strip_tags(&rest[..end]);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Value of an attribute in a tag, e.g. `href = "…"` or `href="…"`.
fn find_attr(tag: &str, name: &str) -> Option<String> {
    let mut cursor = 0;
    while let Some(rel) = tag[cursor..].find(name) {
        let start = cursor + rel;
        cursor = start + name.len();
        let before_ok = start == 0
            || tag[..start]
                .chars()
                .last()
                .is_some_and(|c| c.is_whitespace() || c == '<');
        if !before_ok {
            continue;
        }
        let rest = tag[cursor..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next() else {
            continue;
        };
        if quote != '"' && quote != '\'' {
            continue;
        }
        let value = &rest[1..];
        let Some(end) = value.find(quote) else {
            continue;
        };
        return Some(unescape_attr(&value[..end]));
    }
    None
}

/// Decode the HTML entities WordPress writes into attribute values.
fn unescape_attr(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#038;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
}

/// Plain text of an HTML fragment: tags are dropped, `<br>` becomes ", ".
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        let Some(gt) = rest.find('>') else {
            break;
        };
        if rest.starts_with("<br") {
            out.push_str(", ");
        }
        rest = &rest[gt + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Value of one query-string parameter of a URL (e.g. `doctor=6164`).
fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == name {
                return Some(value.to_string());
            }
        }
    }
    None
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

    const SERVICES_PAGE: &str = r#"
    <div class = "zapis bfon p40 flex step1">
        <div class = "item mb-8">
            <a href = "https://www.cfm.ru/kurgan/zapis/?step=2&service=3&clinic=0" class = "vibor-service flex">
                <span class = "w-500 f-16 lh-13 cblack">Акушеры-гинекологи</span>
                <div class = "flex doctors"><div class = "wh-38 icon oh"><img src = "/a.jpg"></div></div>
            </a>
        </div>
        <div class = "item mb-8">
            <a href = "https://www.cfm.ru/kurgan/zapis/?step=2&service=4&clinic=0" class = "vibor-service flex">
                <span class = "w-500 f-16 lh-13 cblack">Гинекологи-репродуктологи</span>
            </a>
        </div>
        <div class = "item mb-8">
            <a href = "https://www.cfm.ru/kurgan/zapis/?step=2&service=1089&clinic=0" class = "vibor-service flex">
                <span class = "w-500 f-16 lh-13 cblack">Уролог</span>
            </a>
        </div>
    </div>
    "#;

    const SERVICE_PAGE: &str = r#"
    <div class = "zapis bfon p40 flex step2">
        <div class = "bottom grow-1">
            <div class = "doc-item re mb-8">
                <a href = "https://www.cfm.ru/kurgan/zapis/?step=2&service=3&clinic=0&doctor=6154" class = "absolute-a"></a>
                <div class = "back  bwhite br-16 re p20-24">
                    <div class = "flex align-c mb-16">
                        <div class = "text-block">
                            <div class = "rating mb-4"><span>5.0</span></div>
                            <div class = "title f-16 mb-4 line-2">Кузнецова Ольга Николаевна</div>
                            <div class = "f-14 cgray lh-13 line-1">Главный врач, врач акушер-гинеколог</div>
                        </div>
                    </div>
                    <div class = "row-5 f-14 cgray lh-13 flow-rw mb-m10 align-c">
                        <div class = "flex align-c mb-10 mr-10 p15"><span class = "ml-8">ул. Карбышева, 39</span></div>
                        <div class = "flex align-c mb-10 mr-10 p15"><span class = "ml-8">от 2500 ₽</span></div>
                    </div>
                </div>
            </div>
            <div class = "doc-item re mb-8">
                <a href = "https://www.cfm.ru/kurgan/zapis/?step=2&service=3&clinic=0&doctor=6164" class = "absolute-a"></a>
                <div class = "back  bwhite br-16 re p20-24">
                    <div class = "flex align-c mb-16">
                        <div class = "text-block">
                            <div class = "title f-16 mb-4 line-2">Коркина Ольга Владимировна</div>
                            <div class = "f-14 cgray lh-13 line-1">Врач акушер-гинеколог высшей категории, репродуктолог</div>
                        </div>
                    </div>
                    <div class = "row-5 f-14 cgray lh-13 flow-rw mb-m10 align-c">
                        <div class = "flex align-c mb-10 mr-10 p15"><span class = "ml-8">ул. Карбышева, 39</span></div>
                        <div class = "flex align-c mb-10 mr-10 p15"><span class = "ml-8">от 2500 ₽</span></div>
                    </div>
                </div>
            </div>
        </div>
    </div>
    "#;

    const REPIN_SERVICE_PAGE: &str = r#"
    <div class = "zapis bfon p40 flex step2">
        <div class = "bottom grow-1">
            <div class = "doc-item re mb-8">
                <a href = "https://www.cfm.ru/kurgan/zapis/?step=2&service=1089&clinic=0&doctor=7188" class = "absolute-a"></a>
                <div class = "back  bwhite br-16 re p20-24">
                    <div class = "flex align-c mb-16">
                        <div class = "text-block">
                            <div class = "title f-16 mb-4 line-2">Репин Сергей Сергеевич</div>
                            <div class = "f-14 cgray lh-13 line-1">Врач-уролог</div>
                        </div>
                    </div>
                    <div class = "row-5 f-14 cgray lh-13 flow-rw mb-m10 align-c">
                        <div class = "flex align-c mb-10 mr-10 p15"><span class = "ml-8">ул. Карбышева, 39</span></div>
                        <div class = "flex align-c mb-10 mr-10 p15"><span class = "ml-8">Первичный 2500₽<br>Повторный 2000 ₽</span></div>
                    </div>
                </div>
            </div>
        </div>
    </div>
    "#;

    const DOCTOR_PAGE: &str = r#"
    <script>var tek_data2='';var freeDates=["25.11.2026","02.12.2026"]</script>
    <div class = "zapis bfon p40 flex step2">
        <div class = "bottom grow-1">
            <div class = "doc-item re mb-8">
                <div class = "back  bwhite br-16 re p20-24">
                    <div class = "flex align-c mb-16">
                        <div class = "text-block">
                            <div class = "title f-16 mb-4 line-2">Коркина Ольга Владимировна</div>
                            <div class = "f-14 cgray lh-13 line-1">Врач акушер-гинеколог высшей категории, репродуктолог</div>
                        </div>
                    </div>
                    <div class = "row-5 f-14 cgray lh-13 flow-rw mb-m10 align-c">
                        <div class = "flex align-c mb-10 mr-10 p15"><span class = "ml-8">ул. Карбышева, 39</span></div>
                        <div class = "flex align-c mb-10 mr-10 p15"><span class = "ml-8">от 2500 ₽</span></div>
                    </div>
                </div>
            </div>
        </div>
    </div>
    "#;

    const NO_DOCTOR_PAGE: &str = r#"
    <script>var tek_data2='';var freeDates=[]</script>
    <div class = "zapis bfon p40 flex step2"></div>
    "#;

    const SLOTS_PAGE: &str = r#"
    <div class = "zapis bfon p40 flex step2">
        <div class = "right flex fw-60 flow-cn">
            <div class = "mt-20 pt-20 bt-1 sloti">
                <div class = "row-5 flow-rw mb-m10">
                    <div class = "p15">
                        <a href = "https://www.cfm.ru/kurgan/zapis/?step=3&service=3&clinic=0&doctor=6164&date=2026-11-25&time=12:00" class = "btn btn-white-border"><span>12:00 - 12:45</span></a>
                    </div>
                    <div class = "p15">
                        <a href = "https://www.cfm.ru/kurgan/zapis/?step=3&service=3&clinic=0&doctor=6164&date=2026-11-25&time=09:00" class = "btn btn-white-border"><span>09:00 - 09:45</span></a>
                    </div>
                </div>
            </div>
        </div>
    </div>
    "#;

    const NO_SLOTS_PAGE: &str = r#"
    <div class = "zapis bfon p40 flex step2">
        <div class = "right flex fw-60 flow-cn">
            <div class = "back bwhite br-24 p24">
                <div class = "f-16 cgray lh-14">Свободные окошки на этот день разобрали.</div>
            </div>
        </div>
    </div>
    "#;

    #[test]
    fn parses_services() {
        let services = parse_services(SERVICES_PAGE);
        assert_eq!(services.len(), 3);
        assert_eq!(services[0].service_id, 3);
        assert_eq!(services[0].name, "Акушеры-гинекологи");
        assert_eq!(services[1].service_id, 4);
        assert_eq!(services[2].service_id, 1089);
        assert_eq!(services[2].name, "Уролог");
    }

    #[test]
    fn parses_doctors_and_prices() {
        let doctors = parse_doctors(SERVICE_PAGE, 3, None);
        assert_eq!(doctors.len(), 2);
        assert_eq!(doctors[0].doctor_id, 6154);
        assert_eq!(doctors[0].full_name, "Кузнецова Ольга Николаевна");
        assert_eq!(doctors[0].job, "Главный врач, врач акушер-гинеколог");
        assert_eq!(doctors[0].price, "от 2500");
        assert_eq!(doctors[1].doctor_id, 6164);
        assert_eq!(doctors[1].full_name, "Коркина Ольга Владимировна");
        assert_eq!(doctors[1].price, "от 2500");
    }

    #[test]
    fn parses_price_with_two_tariffs() {
        let doctors = parse_doctors(REPIN_SERVICE_PAGE, 1089, None);
        assert_eq!(doctors.len(), 1);
        assert_eq!(doctors[0].doctor_id, 7188);
        assert_eq!(doctors[0].full_name, "Репин Сергей Сергеевич");
        assert_eq!(doctors[0].job, "Врач-уролог");
        assert_eq!(doctors[0].price, "Первичный 2500, Повторный 2000");
    }

    #[test]
    fn parses_selected_doctor_without_link() {
        let doctors = parse_doctors(DOCTOR_PAGE, 3, Some(6164));
        assert_eq!(doctors.len(), 1);
        assert_eq!(doctors[0].doctor_id, 6164);
        assert_eq!(doctors[0].full_name, "Коркина Ольга Владимировна");
    }

    #[test]
    fn parses_free_dates_and_drops_unknown_entries() {
        assert_eq!(
            parse_free_dates(DOCTOR_PAGE),
            vec!["2026-11-25", "2026-12-02"]
        );
        assert!(parse_free_dates(REPIN_SERVICE_PAGE).is_empty());
        assert!(parse_free_dates("<html></html>").is_empty());
    }

    #[test]
    fn parses_slot_times_sorted_and_deduplicated() {
        assert_eq!(parse_slot_times(SLOTS_PAGE), vec!["09:00", "12:00"]);
        assert!(parse_slot_times(NO_SLOTS_PAGE).is_empty());
    }

    #[test]
    fn pick_prefers_exact_full_name_match() {
        let doctors = parse_doctors(SERVICE_PAGE, 3, None);
        let picked = CfmClient::pick_doctor(&doctors, "коркина ольга владимировна").unwrap();
        assert_eq!(picked.doctor_id, 6164);
    }

    #[test]
    fn pick_accepts_unique_substring_match() {
        let doctors = parse_doctors(SERVICE_PAGE, 3, None);
        let only_korkina: Vec<DoctorItem> = doctors
            .into_iter()
            .filter(|d| d.doctor_id == 6164)
            .collect();
        let picked = CfmClient::pick_doctor(&only_korkina, "Коркина").unwrap();
        assert_eq!(picked.doctor_id, 6164);
    }

    #[test]
    fn pick_rejects_ambiguous_matches() {
        let html = r#"
        <div class = "doc-item re mb-8">
            <a href = "https://www.cfm.ru/kurgan/zapis/?step=2&service=3&clinic=0&doctor=1" class = "absolute-a"></a>
            <div class = "title f-16 mb-4 line-2">Анастасия Ивановна Первая</div>
            <div class = "f-14 cgray lh-13 line-1">врач</div>
        </div>
        <div class = "doc-item re mb-8">
            <a href = "https://www.cfm.ru/kurgan/zapis/?step=2&service=3&clinic=0&doctor=2" class = "absolute-a"></a>
            <div class = "title f-16 mb-4 line-2">Анастасия Петровна Вторая</div>
            <div class = "f-14 cgray lh-13 line-1">врач</div>
        </div>
        "#;
        let doctors = parse_doctors(html, 3, None);
        let err = CfmClient::pick_doctor(&doctors, "Анастасия")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("Первая"));
        assert!(err.contains("Вторая"));
    }

    #[test]
    fn booking_link_uses_city_service_and_doctor() {
        let target = CfmTarget {
            service_id: 3,
            service_name: "Акушеры-гинекологи".into(),
            doctor_id: 6164,
            doctor_name: "Коркина Ольга Владимировна".into(),
            job: "Врач акушер-гинеколог".into(),
            price: "от 2500".into(),
        };
        assert_eq!(
            target.booking_link(),
            "https://www.cfm.ru/kurgan/zapis/?step=2&service=3&clinic=0&doctor=6164"
        );
    }

    #[test]
    fn ru_date_parses_to_iso() {
        assert_eq!(ru_date_to_iso("25.11.2026").as_deref(), Some("2026-11-25"));
        assert_eq!(ru_date_to_iso("02.12.2026").as_deref(), Some("2026-12-02"));
        assert!(ru_date_to_iso("не дата").is_none());
    }

    #[tokio::test]
    async fn list_services_fetches_step1() {
        let server =
            MockServer::start(vec![Route::html_query("/zapis/?step=1", SERVICES_PAGE)]).await;
        let client = CfmClient::with_base_url(server.base_url()).unwrap();

        let services = client.list_services().await.unwrap();

        assert_eq!(services.len(), 3);
        assert_eq!(services[0].name, "Акушеры-гинекологи");
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.requests()[0].target, "/zapis/?step=1");
    }

    #[tokio::test]
    async fn list_services_rejects_page_without_services() {
        let server =
            MockServer::start(vec![Route::html_query("/zapis/?step=1", "<html></html>")]).await;
        let client = CfmClient::with_base_url(server.base_url()).unwrap();

        let err = client.list_services().await.unwrap_err().to_string();

        assert!(err.contains("no services found"));
    }

    #[tokio::test]
    async fn doctor_page_returns_card_and_free_dates() {
        let path = "/zapis/?step=2&service=3&clinic=0&doctor=6164";
        let server = MockServer::start(vec![Route::html_query(path, DOCTOR_PAGE)]).await;
        let client = CfmClient::with_base_url(server.base_url()).unwrap();

        let page = client.doctor_page(3, 6164).await.unwrap();

        assert_eq!(page.doctor.full_name, "Коркина Ольга Владимировна");
        assert_eq!(page.doctor.price, "от 2500");
        assert_eq!(page.free_dates, vec!["2026-11-25", "2026-12-02"]);
        assert_eq!(server.requests()[0].target, path);
    }

    #[tokio::test]
    async fn doctor_page_rejects_unknown_id() {
        let path = "/zapis/?step=2&service=3&clinic=0&doctor=999";
        let server = MockServer::start(vec![Route::html_query(path, NO_DOCTOR_PAGE)]).await;
        let client = CfmClient::with_base_url(server.base_url()).unwrap();

        let err = client.doctor_page(3, 999).await.unwrap_err().to_string();

        assert!(err.contains("doctor id 999 not found"));
    }

    #[tokio::test]
    async fn fetch_available_slots_combines_dates_and_times() {
        let server = MockServer::start(vec![
            Route::html_query("/zapis/?step=2&service=3&clinic=0&doctor=6164", DOCTOR_PAGE),
            Route::html_query(
                "/zapis/?step=2&service=3&clinic=0&doctor=6164&date=2026-11-25",
                SLOTS_PAGE,
            ),
            Route::html_query(
                "/zapis/?step=2&service=3&clinic=0&doctor=6164&date=2026-12-02",
                NO_SLOTS_PAGE,
            ),
        ])
        .await;
        let client = CfmClient::with_base_url(server.base_url()).unwrap();
        let target = CfmTarget {
            service_id: 3,
            service_name: "Акушеры-гинекологи".into(),
            doctor_id: 6164,
            doctor_name: "Коркина Ольга Владимировна".into(),
            job: "Врач акушер-гинеколог".into(),
            price: "от 2500".into(),
        };

        let slots = client.fetch_available_slots(&target).await.unwrap();

        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].date, "2026-11-25");
        assert_eq!(slots[0].time, "09:00");
        assert_eq!(slots[0].service_name, "Акушеры-гинекологи");
        assert_eq!(slots[0].price, "от 2500");
        assert_eq!(slots[0].office_name, OFFICE_NAME);
        assert_eq!(slots[1].date, "2026-11-25");
        assert_eq!(slots[1].time, "12:00");
        assert_eq!(server.requests().len(), 3);
    }

    #[tokio::test]
    async fn resolve_targets_by_name_searches_the_service() {
        let server = MockServer::start(vec![
            Route::html_query("/zapis/?step=1", SERVICES_PAGE),
            Route::html_query("/zapis/?step=2&service=3&clinic=0", SERVICE_PAGE),
        ])
        .await;
        let client = CfmClient::with_base_url(server.base_url()).unwrap();
        let watches = [CfmWatch {
            service_id: 3,
            doctor: "Коркина".into(),
        }];

        let targets = client.resolve_targets(&watches).await.unwrap();

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].service_id, 3);
        assert_eq!(targets[0].service_name, "Акушеры-гинекологи");
        assert_eq!(targets[0].doctor_id, 6164);
        assert_eq!(targets[0].doctor_name, "Коркина Ольга Владимировна");
    }

    #[tokio::test]
    async fn resolve_targets_by_id_reads_the_service_name() {
        let server = MockServer::start(vec![
            Route::html_query("/zapis/?step=2&service=3&clinic=0&doctor=6164", DOCTOR_PAGE),
            Route::html_query("/zapis/?step=1", SERVICES_PAGE),
        ])
        .await;
        let client = CfmClient::with_base_url(server.base_url()).unwrap();
        let watches = [CfmWatch {
            service_id: 3,
            doctor: "6164".into(),
        }];

        let targets = client.resolve_targets(&watches).await.unwrap();

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].service_name, "Акушеры-гинекологи");
        assert_eq!(targets[0].doctor_name, "Коркина Ольга Владимировна");
    }

    #[tokio::test]
    async fn resolve_targets_watches_several_doctors() {
        let server = MockServer::start(vec![
            Route::html_query("/zapis/?step=1", SERVICES_PAGE),
            Route::html_query("/zapis/?step=2&service=3&clinic=0&doctor=6164", DOCTOR_PAGE),
            Route::html_query(
                "/zapis/?step=2&service=1089&clinic=0&doctor=7188",
                REPIN_SERVICE_PAGE,
            ),
        ])
        .await;
        let client = CfmClient::with_base_url(server.base_url()).unwrap();
        let watches = [
            CfmWatch {
                service_id: 3,
                doctor: "6164".into(),
            },
            CfmWatch {
                service_id: 1089,
                doctor: "7188".into(),
            },
        ];

        let targets = client.resolve_targets(&watches).await.unwrap();

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].doctor_name, "Коркина Ольга Владимировна");
        assert_eq!(targets[0].service_name, "Акушеры-гинекологи");
        assert_eq!(targets[1].doctor_name, "Репин Сергей Сергеевич");
        assert_eq!(targets[1].service_name, "Уролог");
        assert_eq!(targets[1].price, "Первичный 2500, Повторный 2000");
    }
}
