mod config;
mod harizma;
mod invitro;
mod notifier;
mod slots;
mod state;
mod telegram;

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{FixedOffset, Utc};
use config::Provider;
use harizma::{HarizmaClient, HarizmaTarget};
use invitro::{InvitroClient, WatchTarget};
use notifier::MessageCtx;
use slots::AvailableSlot;
use state::SeenSlots;
use tokio::time::sleep;
use tracing::{error, info, warn};

const YEKT: i32 = 5 * 3600;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let check = args.iter().any(|a| a == "--check");
    let once = args.iter().any(|a| a == "--once");

    if let Some(pos) = args.iter().position(|a| a == "--find") {
        let query = args[pos + 1..].join(" ");
        return find_mode(&query).await;
    }

    if check {
        return check_mode().await;
    }

    let cfg = config::Config::from_env()?;
    match cfg.provider {
        Provider::Invitro => run_watch_invitro(&cfg, once).await,
        Provider::Harizma => run_watch_harizma(&cfg, once).await,
    }
}

// ---------- Invitro watch loop ----------

async fn run_watch_invitro(cfg: &config::Config, once: bool) -> Result<()> {
    let client = InvitroClient::new()?;
    let target = resolve_with_retry(&client, cfg).await?;
    info!(
        "watching {} (bitrix {}), city {}, specialty {}, {} service(s)",
        target.doctor_name,
        target.doctor_bitrix_id,
        target.city.name,
        target.specialty_name,
        target.entries.len()
    );

    let telegram =
        telegram::Telegram::new(&cfg.telegram_bot_token, &cfg.telegram_chat_ids, cfg.dry_run)?;
    let mut seen = SeenSlots::load(&cfg.state_file)?;
    if !seen.existed_before() {
        info!("no state file yet, first run");
    }

    loop {
        if let Err(e) = run_cycle_invitro(&client, cfg, &target, &telegram, &mut seen).await {
            error!("poll cycle failed: {e:#}");
        }
        if once {
            break;
        }
        sleep(Duration::from_secs(cfg.poll_interval_secs)).await;
    }
    Ok(())
}

// ---------- Harizma watch loop ----------

async fn run_watch_harizma(cfg: &config::Config, once: bool) -> Result<()> {
    let client = HarizmaClient::new()?;
    let target = resolve_harizma_with_retry(&client, cfg).await?;
    info!(
        "watching {} (id {}), {}, booking {}",
        target.doctor_name,
        target.doctor_id,
        target.job,
        target.booking_link()
    );

    let telegram =
        telegram::Telegram::new(&cfg.telegram_bot_token, &cfg.telegram_chat_ids, cfg.dry_run)?;
    let mut seen = SeenSlots::load(&cfg.state_file)?;
    if !seen.existed_before() {
        info!("no state file yet, first run");
    }

    loop {
        if let Err(e) = run_cycle_harizma(&client, cfg, &target, &telegram, &mut seen).await {
            error!("poll cycle failed: {e:#}");
        }
        if once {
            break;
        }
        sleep(Duration::from_secs(cfg.poll_interval_secs)).await;
    }
    Ok(())
}

/// Resolve the invitro watch target at startup, retrying on failure (the site
/// may be temporarily unavailable or DDoS-Guard may be active).
async fn resolve_with_retry(client: &InvitroClient, cfg: &config::Config) -> Result<WatchTarget> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let result = match &cfg.doctor_name {
            Some(name) => resolve_by_name(client, cfg, name).await,
            None => {
                client
                    .resolve_target(
                        cfg.doctor_bitrix_id,
                        &cfg.city_slug,
                        cfg.specialty_slug.as_deref(),
                        &cfg.service_ids,
                    )
                    .await
            }
        };
        match result {
            Ok(t) => return Ok(t),
            Err(e) if attempt < 5 => {
                warn!("resolve attempt {attempt} failed: {e:#}; retrying in 30s");
                sleep(Duration::from_secs(30)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Resolve the harizma watch target at startup, retrying on failure.
async fn resolve_harizma_with_retry(
    client: &HarizmaClient,
    cfg: &config::Config,
) -> Result<HarizmaTarget> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let result = client
            .resolve_target(
                cfg.harizma_doctor_id,
                &cfg.harizma_page_path,
                cfg.harizma_doctor_name.as_deref(),
            )
            .await;
        match result {
            Ok(t) => return Ok(t),
            Err(e) if attempt < 5 => {
                warn!("harizma resolve attempt {attempt} failed: {e:#}; retrying in 30s");
                sleep(Duration::from_secs(30)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Resolve the doctor by name (INVITRO_DOCTOR_NAME) instead of a bitrix id.
async fn resolve_by_name(
    client: &InvitroClient,
    cfg: &config::Config,
    name: &str,
) -> Result<WatchTarget> {
    let city = client.city_by_slug(&cfg.city_slug).await?;
    let doctors = client.search_doctors(&city, name).await?;
    let doctor = InvitroClient::pick_doctor(&doctors, name)?;
    info!(
        "resolved doctor by name: {} (bitrix {})",
        doctor.full_name, doctor.bitrix_id
    );
    client
        .resolve_target(
            doctor.bitrix_id,
            &cfg.city_slug,
            cfg.specialty_slug.as_deref(),
            &cfg.service_ids,
        )
        .await
}

// ---------- poll cycles ----------

/// One poll cycle for Invitro: fetch all available slots, notify about new
/// ones, persist state.
async fn run_cycle_invitro(
    client: &InvitroClient,
    cfg: &config::Config,
    target: &WatchTarget,
    telegram: &telegram::Telegram,
    seen: &mut SeenSlots,
) -> Result<()> {
    let today = today_yekt();
    let slots = client.fetch_available_slots(target, &today).await?;
    let link = target.booking_link();
    let ctx = MessageCtx {
        doctor_name: &target.doctor_name,
        booking_link: &link,
    };
    process_new_slots(cfg, telegram, seen, ctx, slots).await
}

/// One poll cycle for Harizma: the site exposes only the doctor's next
/// appointment date, so the result is 0 or 1 date-only slot.
async fn run_cycle_harizma(
    client: &HarizmaClient,
    cfg: &config::Config,
    target: &HarizmaTarget,
    telegram: &telegram::Telegram,
    seen: &mut SeenSlots,
) -> Result<()> {
    let today = today_yekt();
    let slots = client.fetch_available_slots(target, &today).await?;
    let link = target.booking_link();
    let ctx = MessageCtx {
        doctor_name: &target.doctor_name,
        booking_link: &link,
    };
    process_new_slots(cfg, telegram, seen, ctx, slots).await
}

/// Diff the fetched slots against the seen set, notify about the new ones and
/// persist progress. Shared by both providers.
async fn process_new_slots(
    cfg: &config::Config,
    telegram: &telegram::Telegram,
    seen: &mut SeenSlots,
    ctx: MessageCtx<'_>,
    slots: Vec<AvailableSlot>,
) -> Result<()> {
    info!("cycle: {} slot(s) available", slots.len());

    // First run with no history: either notify about everything that is
    // currently open (default) or silently record it as baseline.
    if !seen.existed_before() && !cfg.notify_on_first_run {
        for s in &slots {
            seen.insert(state::SlotKey::from_slot(s));
        }
        seen.save(&cfg.state_file)?;
        info!("baseline recorded: {} slot(s)", slots.len());
        return Ok(());
    }

    let new: Vec<AvailableSlot> = slots
        .iter()
        .filter(|s| !seen.contains(&state::SlotKey::from_slot(s)))
        .cloned()
        .collect();

    if new.is_empty() {
        return Ok(());
    }

    info!("found {} new slot(s), notifying", new.len());
    notifier::notify_new_slots(telegram, &ctx, &new, |group| {
        for s in group {
            seen.insert(state::SlotKey::from_slot(s));
        }
        seen.save(&cfg.state_file)
    })
    .await
}

// ---------- find & check ----------

/// `--find <name>`: search the configured provider for doctors by name and
/// print their booking pages (no Telegram credentials required). Exits with
/// code 1 if nothing found.
async fn find_mode(query: &str) -> Result<()> {
    match Provider::from_env() {
        Provider::Invitro => find_mode_invitro(query).await,
        Provider::Harizma => find_mode_harizma(query).await,
    }
}

async fn find_mode_invitro(query: &str) -> Result<()> {
    if query.is_empty() {
        anyhow::bail!("usage: invitro-bot --find <doctor name>");
    }
    let client = InvitroClient::new()?;
    let city_slug = std::env::var("INVITRO_CITY_SLUG").unwrap_or_else(|_| "kurgan".into());
    let city = client.city_by_slug(&city_slug).await?;
    let doctors = client.search_doctors(&city, query).await?;

    if doctors.is_empty() {
        eprintln!("No doctors found for '{query}' in {}", city.name);
        std::process::exit(1);
    }

    println!(
        "Found {} doctor(s) for '{query}' in {}:",
        doctors.len(),
        city.name
    );
    for d in &doctors {
        println!(
            "\n{} (bitrix_id={}, uuid={}, стаж {} лет)\n  booking page: {}",
            d.full_name,
            d.bitrix_id,
            d.uuid,
            d.experience,
            InvitroClient::booking_page(&city.slug, d)
        );
    }
    Ok(())
}

async fn find_mode_harizma(query: &str) -> Result<()> {
    if query.is_empty() {
        anyhow::bail!("usage: invitro-bot --find <doctor name>");
    }
    let client = HarizmaClient::new()?;
    let doctors = client.list_doctors().await?;
    let norm = normalize_name(query);
    let matches: Vec<&harizma::DoctorItem> = doctors
        .iter()
        .filter(|d| normalize_name(&d.full_name).contains(&norm))
        .collect();

    if matches.is_empty() {
        eprintln!("No doctors found for '{query}' on harizma45.ru");
        std::process::exit(1);
    }

    println!(
        "Found {} doctor(s) for '{query}' on harizma45.ru:",
        matches.len()
    );
    for d in &matches {
        println!(
            "\n{} (id={}, {})\n  booking page: https://harizma45.ru{}\n  ближайший прием: {}",
            d.full_name,
            d.doctor_id,
            d.job,
            d.page_path,
            d.next_appointment.as_deref().unwrap_or("нет")
        );
    }
    Ok(())
}

/// `--check`: resolve and print the watch target (doctor, city, services)
/// without polling or requiring Telegram credentials.
async fn check_mode() -> Result<()> {
    match Provider::from_env() {
        Provider::Invitro => check_mode_invitro().await,
        Provider::Harizma => check_mode_harizma().await,
    }
}

async fn check_mode_invitro() -> Result<()> {
    let client = InvitroClient::new()?;
    let bitrix: u64 = std::env::var("INVITRO_DOCTOR_BITRIX_ID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(19143);
    let city_slug = std::env::var("INVITRO_CITY_SLUG").unwrap_or_else(|_| "kurgan".into());
    let doctor_name = std::env::var("INVITRO_DOCTOR_NAME")
        .ok()
        .filter(|s| !s.is_empty());
    let specialty = std::env::var("INVITRO_SPECIALTY_SLUG")
        .ok()
        .filter(|s| !s.is_empty());
    let services: Vec<String> = std::env::var("INVITRO_SERVICE_IDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let bitrix_id = if let Some(name) = &doctor_name {
        let city = client.city_by_slug(&city_slug).await?;
        let doctors = client.search_doctors(&city, name).await?;
        let doctor = InvitroClient::pick_doctor(&doctors, name)?;
        println!(
            "Doctor:     {} (bitrix_id={}, resolved by name)",
            doctor.full_name, doctor.bitrix_id
        );
        doctor.bitrix_id
    } else {
        bitrix
    };

    let target = client
        .resolve_target(bitrix_id, &city_slug, specialty.as_deref(), &services)
        .await
        .context("failed to resolve doctor; check INVITRO_DOCTOR_BITRIX_ID / INVITRO_DOCTOR_NAME / INVITRO_CITY_SLUG / INVITRO_SPECIALTY_SLUG")?;

    if doctor_name.is_none() {
        println!(
            "Doctor:     {} (bitrix_id={})",
            target.doctor_name, target.doctor_bitrix_id
        );
    }
    println!("City:       {} ({})", target.city.name, target.city.slug);
    println!("Specialty:  {}", target.specialty_name);
    println!("UUID:       {}", target.doctor_uuid);
    println!("Watching {} office/service pair(s):", target.entries.len());
    for e in &target.entries {
        println!("  - {} | {} | {} ₽", e.office_name, e.service_name, e.price);
    }

    let today = today_yekt();
    let slots = client.fetch_available_slots(&target, &today).await?;
    println!("\nAvailable slots today (window from {today}):");
    if slots.is_empty() {
        println!("  (none)");
    }
    for s in &slots {
        println!(
            "  - {} {} | {} | {}",
            s.date, s.time, s.office_name, s.service_name
        );
    }
    Ok(())
}

async fn check_mode_harizma() -> Result<()> {
    let client = HarizmaClient::new()?;
    let doctor_id: u64 = std::env::var("HARIZMA_DOCTOR_ID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(940);
    let page_path = std::env::var("HARIZMA_PAGE_PATH")
        .unwrap_or_else(|_| "/register/uzi-diagnostika/vozhdaeva-anastasiya/".into());
    let doctor_name = std::env::var("HARIZMA_DOCTOR_NAME")
        .ok()
        .filter(|s| !s.is_empty());

    let (target, by_name) = if let Some(name) = &doctor_name {
        let doctors = client.list_doctors().await?;
        let item = HarizmaClient::pick_doctor(&doctors, name)?;
        (HarizmaTarget::from_item(item), true)
    } else {
        let target = client
            .resolve_target(doctor_id, &page_path, None)
            .await
            .context(
                "failed to resolve doctor; check HARIZMA_DOCTOR_ID / HARIZMA_PAGE_PATH / HARIZMA_DOCTOR_NAME",
            )?;
        (target, false)
    };

    println!(
        "Doctor:     {} (id={}{})",
        target.doctor_name,
        target.doctor_id,
        if by_name { ", resolved by name" } else { "" }
    );
    println!("Specialty:  {}", target.job);
    println!("Booking:    {}", target.booking_link());

    let today = today_yekt();
    let item = client
        .doctor_on_page(&target.page_path, target.doctor_id)
        .await?;
    println!(
        "Ближайший прием: {}",
        item.next_appointment.as_deref().unwrap_or("нет")
    );
    let slots = client.fetch_available_slots(&target, &today).await?;
    println!("\nAvailable slots (window from {today}):");
    if slots.is_empty() {
        println!("  (none)");
    }
    for s in &slots {
        println!("  - {}", s.date);
    }
    Ok(())
}

fn today_yekt() -> String {
    let tz = FixedOffset::east_opt(YEKT).expect("valid offset");
    Utc::now().with_timezone(&tz).format("%Y-%m-%d").to_string()
}

/// Normalize a name for comparison: lowercase and collapse whitespace.
fn normalize_name(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
