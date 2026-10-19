use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::slots::AvailableSlot;

const BASE_URL: &str = "https://www.invitro.ru/golk";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Minimal client for the public Invitro.ru JSON API (`/golk/*`).
///
/// The endpoints are public (no auth). A browser-like User-Agent is sent because
/// the site sits behind DDoS-Guard, which may challenge requests without one.
#[derive(Clone)]
pub struct InvitroClient {
    http: reqwest::Client,
    base_url: String,
}

impl InvitroClient {
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

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(query)
            .header("Accept", "application/json")
            .send()
            .await
            .with_context(|| format!("request failed: {url}"))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{url} -> HTTP {status}: {}", truncate(&body, 300));
        }
        serde_json::from_str(&body)
            .with_context(|| format!("bad JSON from {url}: {}", truncate(&body, 200)))
    }

    async fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .header("Accept", "application/json")
            .send()
            .await
            .with_context(|| format!("request failed: {url}"))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{url} -> HTTP {status}: {}", truncate(&body, 300));
        }
        serde_json::from_str(&body)
            .with_context(|| format!("bad JSON from {url}: {}", truncate(&body, 200)))
    }

    /// Map a numeric (bitrix) doctor id from the page URL to an internal UUID.
    pub async fn doctor_uuid(&self, bitrix_id: u64) -> Result<String> {
        let m: MapperResponse = self
            .get(
                &format!("/doctors/api/v1/mapper/doctors/bitrix/{bitrix_id}"),
                &[],
            )
            .await?;
        Ok(m.uuid)
    }

    /// Find a city by slug (e.g. `kurgan`).
    pub async fn city_by_slug(&self, slug: &str) -> Result<City> {
        let r: CitiesResponse = self.get("/addresses/api/v1/cities", &[("q", slug)]).await?;
        r.cities
            .iter()
            .find(|c| c.slug == slug)
            .or_else(|| r.cities.first())
            .cloned()
            .with_context(|| format!("city with slug '{slug}' not found"))
    }

    /// Search doctors in a city by name (case-insensitive, substring match).
    ///
    /// Uses the site's own search API, so doctors without current availability
    /// are found as well. Returns an empty list when nothing matches.
    pub async fn search_doctors(&self, city: &City, query: &str) -> Result<Vec<FoundDoctor>> {
        let r: SearchResponse = self
            .get(
                "/search/api/v1/search/doctors",
                &[("q", query), ("cityId", &city.id)],
            )
            .await?;
        if r.ids.is_empty() {
            return Ok(Vec::new());
        }
        let items: Vec<DoctorBatchItem> = self
            .post_json("/doctors/api/v1/doctors/batch", json!({ "ids": r.ids }))
            .await?;
        Ok(items
            .into_iter()
            .map(|d| FoundDoctor {
                uuid: d.id,
                bitrix_id: d.bitrix_id,
                full_name: format!("{} {} {}", d.last_name, d.first_name, d.middle_name),
                specialty_slug: d.speciality_slug,
                experience: d.experience,
            })
            .collect())
    }

    /// Pick a single doctor out of search results for a name query.
    ///
    /// Prefers an exact (normalized) full-name match; otherwise accepts a unique
    /// result. Ambiguous queries are rejected with a list of candidates.
    pub fn pick_doctor<'a>(doctors: &'a [FoundDoctor], query: &str) -> Result<&'a FoundDoctor> {
        if doctors.is_empty() {
            anyhow::bail!("no doctor found by name '{query}'");
        }
        let norm = normalize_name(query);
        let exact: Vec<&FoundDoctor> = doctors
            .iter()
            .filter(|d| normalize_name(&d.full_name) == norm)
            .collect();
        if exact.len() == 1 {
            return Ok(exact[0]);
        }
        if doctors.len() == 1 {
            return Ok(&doctors[0]);
        }
        let list = doctors
            .iter()
            .map(|d| format!("  - {} (bitrix {})", d.full_name, d.bitrix_id))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!("ambiguous doctor name '{query}', matches:\n{list}")
    }

    /// Booking page URL of a doctor, e.g.
    /// `https://www.invitro.ru/kurgan/vrachi/ginekolog/19143/`.
    pub fn booking_page(city_slug: &str, doctor: &FoundDoctor) -> String {
        format!(
            "https://www.invitro.ru/{city_slug}/vrachi/{}/{}",
            doctor.specialty_slug, doctor.bitrix_id
        )
    }

    pub async fn doctor(&self, uuid: &str, city_id: &str) -> Result<Doctor> {
        self.get(
            &format!("/appointments/api/v1/doctors/{uuid}/"),
            &[("cityID", city_id)],
        )
        .await
    }

    /// Offices and services of a doctor for one specialty in one city.
    pub async fn doctor_specialty(
        &self,
        doctor_uuid: &str,
        specialty_id: &str,
        city_id: &str,
    ) -> Result<DoctorSpecialty> {
        self.get(
            &format!("/appointments/api/v1/doctors/{doctor_uuid}/specialties/{specialty_id}/"),
            &[("cityID", city_id)],
        )
        .await
    }

    /// Office details, used to verify which city an office belongs to.
    pub async fn office(&self, office_id: &str) -> Result<OfficeDetails> {
        self.get(
            &format!("/addresses/api/v1/medical-offices/{office_id}"),
            &[],
        )
        .await
    }

    /// Availability per date (today .. ~30 days) for an office/doctor/service.
    /// `slot_date` is the start of the window (usually today in the target city).
    pub async fn intervals(
        &self,
        office_id: &str,
        doctor_uuid: &str,
        service_id: &str,
        slot_date: &str,
    ) -> Result<BTreeMap<String, bool>> {
        let r: IntervalsResponse = self
            .get(
                "/appointments/api/v1/intervals/doctors",
                &[
                    ("officeID", office_id),
                    ("doctorID", doctor_uuid),
                    ("serviceID", service_id),
                    ("slotDate", slot_date),
                ],
            )
            .await?;
        Ok(r.intervals)
    }

    /// Concrete times of one date for an office/doctor/service.
    pub async fn slots(
        &self,
        office_id: &str,
        doctor_uuid: &str,
        service_id: &str,
        slot_date: &str,
    ) -> Result<SlotsResponse> {
        self.get(
            "/appointments/api/v1/slots/doctors",
            &[
                ("officeID", office_id),
                ("doctorID", doctor_uuid),
                ("serviceID", service_id),
                ("slotDate", slot_date),
            ],
        )
        .await
    }
}

// ---------- payload types ----------

#[derive(Debug, Deserialize)]
struct MapperResponse {
    uuid: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct City {
    pub id: String,
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Deserialize)]
struct CitiesResponse {
    cities: Vec<City>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DoctorBatchItem {
    pub id: String,
    pub bitrix_id: u64,
    pub first_name: String,
    pub middle_name: String,
    pub last_name: String,
    #[serde(default)]
    pub speciality_slug: String,
    #[serde(default)]
    pub experience: u64,
}

/// A doctor found by name search, with enough data to build the booking page URL.
#[derive(Debug, Clone)]
pub struct FoundDoctor {
    pub uuid: String,
    pub bitrix_id: u64,
    pub full_name: String,
    pub specialty_slug: String,
    pub experience: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Specialty {
    pub id: String,
    pub slug: String,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub is_primary: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Doctor {
    pub first_name: String,
    pub middle_name: String,
    pub last_name: String,
    pub specialties: Vec<Specialty>,
}

impl Doctor {
    pub fn full_name(&self) -> String {
        format!(
            "{} {} {}",
            self.last_name, self.first_name, self.middle_name
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DoctorSpecialty {
    #[serde(default)]
    pub offices: Vec<OfficeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OfficeEntry {
    pub office: OfficeInfo,
    pub main_service_id: String,
    #[serde(default)]
    pub services: Vec<Service>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OfficeInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    pub service_id: String,
    pub name: String,
    #[serde(default)]
    pub price: String,
    #[serde(default)]
    pub is_consultation: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OfficeDetails {
    pub city_id: String,
    #[serde(default)]
    pub city_name: String,
}

#[derive(Debug, Deserialize)]
struct IntervalsResponse {
    intervals: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlotsResponse {
    #[serde(default)]
    pub slots: Vec<String>,
}

// ---------- high-level watch model ----------

/// A service of the doctor at a concrete office that we poll.
#[derive(Debug, Clone)]
pub struct WatchEntry {
    pub office_id: String,
    pub office_name: String,
    pub service_id: String,
    pub service_name: String,
    pub price: String,
}

/// Everything needed to poll the doctor, resolved once per cycle.
#[derive(Debug, Clone)]
pub struct WatchTarget {
    pub doctor_uuid: String,
    pub doctor_bitrix_id: u64,
    pub doctor_name: String,
    pub city: City,
    pub specialty_name: String,
    pub specialty_slug: Option<String>,
    pub entries: Vec<WatchEntry>,
}

impl WatchTarget {
    /// Booking page URL of the watched doctor, e.g.
    /// `https://www.invitro.ru/kurgan/vrachi/ginekolog/19143/`.
    pub fn booking_link(&self) -> String {
        let slug = self
            .specialty_slug
            .as_deref()
            .unwrap_or("ginekolog")
            .to_lowercase();
        format!(
            "https://www.invitro.ru/{}/vrachi/{}/{}/",
            self.city.slug, slug, self.doctor_bitrix_id
        )
    }
}

impl InvitroClient {
    /// Resolve doctor + city + specialty + offices/services into poll targets.
    ///
    /// Only offices belonging to `city_slug` are kept. `service_ids` (if non-empty)
    /// selects exact services; otherwise the doctor's main service at each office
    /// is watched (the one used by the site's booking flow).
    pub async fn resolve_target(
        &self,
        bitrix_id: u64,
        city_slug: &str,
        specialty_slug: Option<&str>,
        service_ids: &[String],
    ) -> Result<WatchTarget> {
        let city = self.city_by_slug(city_slug).await?;
        let uuid = self.doctor_uuid(bitrix_id).await?;
        let doctor = self.doctor(&uuid, &city.id).await?;

        let specialty = match specialty_slug {
            Some(slug) => doctor
                .specialties
                .iter()
                .find(|s| s.slug == slug)
                .with_context(|| format!("doctor {bitrix_id} has no specialty '{slug}'"))?
                .clone(),
            None => doctor
                .specialties
                .iter()
                .find(|s| s.is_primary)
                .or_else(|| doctor.specialties.first())
                .context("doctor has no specialties")?
                .clone(),
        };

        let ds = self
            .doctor_specialty(&uuid, &specialty.id, &city.id)
            .await?;

        let mut entries = Vec::new();
        for off in &ds.offices {
            // Verify the office really is in the target city (the API may return
            // offices of other cities for the same doctor).
            let details = self.office(&off.office.id).await?;
            if details.city_id != city.id {
                tracing::debug!(
                    "skipping office {} ({}) — not in {}",
                    off.office.name,
                    details.city_name,
                    city.name
                );
                continue;
            }

            // By default watch only the main service — the one the site's
            // "Записаться" flow uses (and what the doctor page shows), so the
            // user gets one message per office instead of near-duplicates for
            // every consultation service.
            let selected: Vec<&Service> = if service_ids.is_empty() {
                let main: Vec<&Service> = off
                    .services
                    .iter()
                    .filter(|s| s.service_id == off.main_service_id)
                    .collect();
                if !main.is_empty() {
                    main
                } else {
                    let consult: Vec<&Service> =
                        off.services.iter().filter(|s| s.is_consultation).collect();
                    if consult.is_empty() {
                        off.services.iter().collect()
                    } else {
                        consult
                    }
                }
            } else {
                off.services
                    .iter()
                    .filter(|s| service_ids.iter().any(|id| id == &s.service_id))
                    .collect()
            };

            for svc in selected {
                entries.push(WatchEntry {
                    office_id: off.office.id.clone(),
                    office_name: off.office.name.clone(),
                    service_id: svc.service_id.clone(),
                    service_name: svc.name.clone(),
                    price: svc.price.clone(),
                });
            }
        }

        if entries.is_empty() {
            anyhow::bail!(
                "doctor {bitrix_id} has no watched services in {city_slug} (office/service filter too narrow?)"
            );
        }

        Ok(WatchTarget {
            doctor_uuid: uuid,
            doctor_bitrix_id: bitrix_id,
            doctor_name: doctor.full_name(),
            city,
            specialty_name: specialty
                .custom_name
                .clone()
                .or(specialty.name)
                .unwrap_or_else(|| specialty.slug.clone()),
            specialty_slug: Some(specialty.slug.clone()),
            entries,
        })
    }

    /// Fetch all currently available slots for all watched entries.
    pub async fn fetch_available_slots(
        &self,
        target: &WatchTarget,
        today: &str,
    ) -> Result<Vec<AvailableSlot>> {
        let mut out = BTreeSet::new();

        for entry in &target.entries {
            let intervals = match self
                .intervals(
                    &entry.office_id,
                    &target.doctor_uuid,
                    &entry.service_id,
                    today,
                )
                .await
            {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!("intervals failed for {}: {e}", entry.service_name);
                    continue;
                }
            };

            let available_dates: Vec<&String> = intervals
                .iter()
                .filter(|(_, available)| **available)
                .map(|(d, _)| d)
                .collect();

            for date in available_dates {
                let slots = match self
                    .slots(
                        &entry.office_id,
                        &target.doctor_uuid,
                        &entry.service_id,
                        date,
                    )
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("slots failed for {} at {date}: {e}", entry.service_name);
                        continue;
                    }
                };
                for time in slots.slots {
                    out.insert(AvailableSlot {
                        office_id: entry.office_id.clone(),
                        office_name: entry.office_name.clone(),
                        service_id: entry.service_id.clone(),
                        service_name: entry.service_name.clone(),
                        price: entry.price.clone(),
                        date: date.clone(),
                        time,
                    });
                }
            }
        }

        Ok(out.into_iter().collect())
    }
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

    fn found(bitrix: u64, name: &str) -> FoundDoctor {
        FoundDoctor {
            uuid: format!("uuid-{bitrix}"),
            bitrix_id: bitrix,
            full_name: name.to_string(),
            specialty_slug: "ginekolog".into(),
            experience: 10,
        }
    }

    #[test]
    fn pick_prefers_exact_full_name_match() {
        let docs = vec![
            found(1, "Коркина Ольга Владимировна"),
            found(2, "Хохлова Ольга Евгеньевна"),
        ];
        let picked = InvitroClient::pick_doctor(&docs, "хохлова ольга евгеньевна").unwrap();
        assert_eq!(picked.bitrix_id, 2);
    }

    #[test]
    fn pick_accepts_unique_substring_match() {
        let docs = vec![found(19143, "Хохлова Ольга Евгеньевна")];
        let picked = InvitroClient::pick_doctor(&docs, "Хохлова").unwrap();
        assert_eq!(picked.bitrix_id, 19143);
    }

    #[test]
    fn pick_rejects_ambiguous_matches() {
        let docs = vec![
            found(1, "Коркина Ольга Владимировна"),
            found(2, "Хохлова Ольга Евгеньевна"),
        ];
        let err = InvitroClient::pick_doctor(&docs, "Ольга")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("Коркина"));
        assert!(err.contains("Хохлова"));
    }

    #[test]
    fn pick_rejects_empty() {
        let err = InvitroClient::pick_doctor(&[], "никто")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no doctor found"));
    }

    #[test]
    fn booking_page_uses_slug_and_bitrix_id() {
        let d = found(19143, "Хохлова Ольга Евгеньевна");
        assert_eq!(
            InvitroClient::booking_page("kurgan", &d),
            "https://www.invitro.ru/kurgan/vrachi/ginekolog/19143"
        );
    }

    #[tokio::test]
    async fn search_doctors_maps_batch_items() {
        let server = MockServer::start(vec![
            Route::json(
                "/search/api/v1/search/doctors",
                r#"{"group":"doctors","ids":["uuid-1","uuid-2"]}"#,
            ),
            Route::json(
                "/doctors/api/v1/doctors/batch",
                r#"[
                    {"id":"uuid-1","bitrix_id":19143,"first_name":"Ольга",
                     "middle_name":"Евгеньевна","last_name":"Хохлова",
                     "speciality_slug":"ginekolog","experience":28},
                    {"id":"uuid-2","bitrix_id":7,"first_name":"Иван",
                     "middle_name":"Сергеевич","last_name":"Петров"}
                ]"#,
            ),
        ])
        .await;
        let client = InvitroClient::with_base_url(server.base_url()).unwrap();
        let city = City {
            id: "city-1".into(),
            name: "Курган".into(),
            slug: "kurgan".into(),
        };

        let doctors = client.search_doctors(&city, "Хохлова").await.unwrap();

        assert_eq!(doctors.len(), 2);
        assert_eq!(doctors[0].uuid, "uuid-1");
        assert_eq!(doctors[0].bitrix_id, 19143);
        assert_eq!(doctors[0].full_name, "Хохлова Ольга Евгеньевна");
        assert_eq!(doctors[0].specialty_slug, "ginekolog");
        assert_eq!(doctors[0].experience, 28);
        assert_eq!(doctors[1].full_name, "Петров Иван Сергеевич");
        assert_eq!(doctors[1].specialty_slug, "");
        assert_eq!(doctors[1].experience, 0);

        // The name query is scoped to the city.
        let requests = server.requests();
        let search = requests
            .iter()
            .find(|r| r.target.starts_with("/search/api/v1/search/doctors"))
            .expect("search request");
        assert_eq!(search.method, "GET");
        assert!(search.target.contains("q="));
        assert!(search.target.contains("cityId=city-1"));

        let batch = requests
            .iter()
            .find(|r| r.target == "/doctors/api/v1/doctors/batch")
            .expect("batch request");
        assert_eq!(batch.method, "POST");
        let sent: serde_json::Value = serde_json::from_str(&batch.body).unwrap();
        assert_eq!(sent["ids"], serde_json::json!(["uuid-1", "uuid-2"]));
    }

    #[tokio::test]
    async fn search_doctors_returns_empty_without_ids() {
        let server = MockServer::start(vec![Route::json(
            "/search/api/v1/search/doctors",
            r#"{"group":"doctors","ids":[]}"#,
        )])
        .await;
        let client = InvitroClient::with_base_url(server.base_url()).unwrap();
        let city = City {
            id: "city-1".into(),
            name: "Курган".into(),
            slug: "kurgan".into(),
        };

        let doctors = client.search_doctors(&city, "Никто").await.unwrap();

        assert!(doctors.is_empty());
        // No ids: the batch request must not be sent.
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn city_by_slug_prefers_exact_match() {
        let server = MockServer::start(vec![Route::json(
            "/addresses/api/v1/cities",
            r#"{"cities":[
                {"id":"1","name":"Матвеев Курган","slug":"matveev-kurgan"},
                {"id":"2","name":"Курган","slug":"kurgan"}
            ]}"#,
        )])
        .await;
        let client = InvitroClient::with_base_url(server.base_url()).unwrap();

        let city = client.city_by_slug("kurgan").await.unwrap();

        assert_eq!(city.id, "2");
        assert_eq!(city.name, "Курган");
    }

    #[tokio::test]
    async fn city_by_slug_rejects_empty_response() {
        let server = MockServer::start(vec![Route::json(
            "/addresses/api/v1/cities",
            r#"{"cities":[]}"#,
        )])
        .await;
        let client = InvitroClient::with_base_url(server.base_url()).unwrap();

        let err = client
            .city_by_slug("nowhere")
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("city with slug 'nowhere' not found"));
    }
}
