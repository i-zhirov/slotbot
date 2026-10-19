use std::collections::BTreeMap;

use anyhow::Result;

use crate::slots::AvailableSlot;
use crate::telegram::Telegram;

/// Telegram message length limit (4096 chars); stay safely below it.
const MAX_MESSAGE_CHARS: usize = 3500;

/// Everything the message formatter needs from the watch target:
/// the doctor's name and the booking link (provider-specific).
pub struct MessageCtx<'a> {
    pub doctor_name: &'a str,
    pub booking_link: &'a str,
}

/// Send one Telegram message per (office, service) group of new slots.
///
/// `mark_sent` is invoked after each group is fully delivered, so the caller
/// can persist progress and only retry the unsent groups later.
pub async fn notify_new_slots<F>(
    telegram: &Telegram,
    ctx: &MessageCtx<'_>,
    slots: &[AvailableSlot],
    mut mark_sent: F,
) -> Result<()>
where
    F: FnMut(&[&AvailableSlot]) -> Result<()>,
{
    let mut groups: BTreeMap<(String, String, String, String, String), Vec<&AvailableSlot>> =
        BTreeMap::new();
    for s in slots {
        groups
            .entry((
                s.office_id.clone(),
                s.office_name.clone(),
                s.service_id.clone(),
                s.service_name.clone(),
                s.price.clone(),
            ))
            .or_default()
            .push(s);
    }

    for ((_, office_name, _, service_name, price), group) in &groups {
        for msg in format_messages(ctx, office_name, service_name, price, group) {
            telegram.send(&msg).await?;
        }
        mark_sent(group)?;
    }
    Ok(())
}

/// Build the messages for one (office, service) group, chunking date lines so
/// each message stays under Telegram's length limit.
fn format_messages(
    ctx: &MessageCtx,
    office_name: &str,
    service_name: &str,
    price: &str,
    group: &[&AvailableSlot],
) -> Vec<String> {
    let mut by_date: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for s in group {
        // Slots without an exact time (harizma exposes only the next
        // appointment date) keep the date but contribute no time entries.
        let entry = by_date.entry(&s.date).or_default();
        if !s.time.is_empty() {
            entry.push(&s.time);
        }
    }

    // The price line doubles as the service line; keep it only when the
    // provider reports a price (harizma has no prices on the booking page).
    let price_line = if price.is_empty() {
        String::new()
    } else {
        format!("💳 {service_name} — {price} ₽\n")
    };

    let header = format!(
        "🩺 <b>Новые слоты у врача: {}</b>\n🏥 {}\n{price_line}",
        ctx.doctor_name, office_name
    );

    let mut messages = Vec::new();
    let mut current = String::with_capacity(MAX_MESSAGE_CHARS + 128);
    current.push_str(&header);

    for (date, times) in &by_date {
        let line = if times.is_empty() {
            // harizma exposes only the next-appointment date, no exact time.
            format!("📅 {date}\n")
        } else {
            let mut sorted = times.clone();
            sorted.sort();
            format!("📅 {date}: {}\n", sorted.join(", "))
        };

        // Start a new message if this line does not fit; always keep the header
        // on subsequent chunks with a "(продолжение)" marker.
        if current.len() + line.len() > MAX_MESSAGE_CHARS {
            current.push_str(&format!("🔗 {}", ctx.booking_link));
            messages.push(current);
            current = String::with_capacity(MAX_MESSAGE_CHARS + 128);
            current.push_str(&header);
            current.push_str("(продолжение)\n");
        }
        current.push_str(&line);
    }

    current.push_str(&format!("🔗 {}", ctx.booking_link));
    messages.push(current);
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> MessageCtx<'static> {
        MessageCtx {
            doctor_name: "Хохлова Ольга Евгеньевна",
            booking_link: "https://www.invitro.ru/kurgan/vrachi/ginekolog/19143/",
        }
    }

    fn slot(date: &str, time: &str) -> AvailableSlot {
        AvailableSlot {
            office_id: "office".into(),
            office_name: "МО Курган-4:Гоголя 133".into(),
            service_id: "svc".into(),
            service_name: "Прием врача в клинике".into(),
            price: "3500.00".into(),
            date: date.into(),
            time: time.into(),
        }
    }

    #[test]
    fn message_contains_link_and_slots() {
        let msgs = format_messages(
            &ctx(),
            "МО Курган-4:Гоголя 133",
            "Прием врача в клинике",
            "3500.00",
            &[
                &slot("2026-10-05", "13:00"),
                &slot("2026-10-05", "10:00"),
                &slot("2026-10-06", "09:30"),
            ],
        );
        assert_eq!(msgs.len(), 1);
        let msg = &msgs[0];
        assert!(msg.contains("https://www.invitro.ru/kurgan/vrachi/ginekolog/19143/"));
        assert!(msg.contains("📅 2026-10-05: 10:00, 13:00"));
        assert!(msg.contains("📅 2026-10-06: 09:30"));
        assert!(msg.contains("3500.00"));
        assert!(msg.contains("\n🔗 https://www.invitro.ru/kurgan/vrachi/ginekolog/19143/"));
        assert!(msg.ends_with("19143/"));
        assert!(!msg.contains("vrachi/vrachi"));
    }

    #[test]
    fn harizma_style_date_only_slot() {
        let s = AvailableSlot {
            office_id: "harizma-940".into(),
            office_name: "МЦ «Харизма», г. Курган, ул. Куйбышева, 27".into(),
            service_id: "online".into(),
            service_name: "Онлайн-запись".into(),
            price: String::new(),
            date: "2026-10-19".into(),
            time: String::new(),
        };
        let msgs = format_messages(
            &MessageCtx {
                doctor_name: "Вождаева Анастасия Викторовна",
                booking_link: "https://harizma45.ru/register/uzi-diagnostika/vozhdaeva-anastasiya/",
            },
            "МЦ «Харизма», г. Курган, ул. Куйбышева, 27",
            "Онлайн-запись",
            "",
            &[&s],
        );
        assert_eq!(msgs.len(), 1);
        let msg = &msgs[0];
        assert!(msg.contains("Вождаева Анастасия Викторовна"));
        assert!(msg.contains("📅 2026-10-19"));
        assert!(!msg.contains("2026-10-19: ")); // no dangling colon for date-only slots
        assert!(!msg.contains("—  ₽")); // no dangling price line
        assert!(!msg.contains("💳"));
        assert!(msg.contains("https://harizma45.ru/register/uzi-diagnostika/vozhdaeva-anastasiya/"));
    }

    #[test]
    fn long_group_is_chunked() {
        let slots: Vec<AvailableSlot> = (0..300)
            .flat_map(|d| {
                (0..10).map(move |t| {
                    slot(
                        &format!("2026-11-{:02}", (d % 28) + 1),
                        &format!("{:02}:00", t + 9),
                    )
                })
            })
            .collect();
        let refs: Vec<&AvailableSlot> = slots.iter().collect();
        let msgs = format_messages(
            &ctx(),
            "МО Курган-4:Гоголя 133",
            "Прием врача в клинике",
            "3500.00",
            &refs,
        );
        assert!(
            msgs.len() > 1,
            "expected multiple messages, got {}",
            msgs.len()
        );
        for m in &msgs {
            assert!(
                m.len() <= MAX_MESSAGE_CHARS + 128,
                "message too long: {}",
                m.len()
            );
            assert!(m.contains("https://www.invitro.ru/kurgan/vrachi/ginekolog/19143/"));
        }
    }
}
