# slotbot

Telegram bot that watches clinic appointment systems and notifies you when
new slots appear for a doctor. Several providers are supported:

- **Invitro.ru** (default): <https://www.invitro.ru/kurgan/vrachi/ginekolog/19143/>
  — Хохлова Ольга Евгеньевна, гинеколог, МО Курган-4 (Гоголя 133).
- **harizma45.ru** (МЦ «Харизма», Курган, ул. Куйбышева, 27):
  <https://harizma45.ru/register/uzi-diagnostika/vozhdaeva-anastasiya/>
  — Вождаева Анастасия Викторовна, врач УЗИ-диагностики.
- **cfm.ru** (ЦСМ, Курган, ул. Карбышева, 39):
  <https://www.cfm.ru/kurgan/doctors/korkina-olga-vladimirovna/>
  — Коркина Ольга Владимировна, врач акушер-гинеколог, репродуктолог.
  Several doctors can be watched at once, e.g. Репин Сергей Сергеевич,
  врач-уролог, with `CFM_DOCTORS=3:6164,1089:7188`.

Select the provider with `SLOTBOT_PROVIDER` (`invitro` default, `harizma`, `cfm`).

Written in Rust: a single static binary (~10 MB), no runtime dependencies —
suitable for a small NixOS VPS. Polls the clinic site every
`SLOTBOT_POLL_INTERVAL_SECS` (default 120 s) with a browser-like User-Agent; no
authentication or cookies are required.

## How it works

### Invitro.ru

The Invitro.ru frontend is a SPA backed by a public API at `https://www.invitro.ru/golk`.
The bot reproduces the flow the web page uses:

1. `GET /doctors/api/v1/mapper/doctors/bitrix/{id}` — map the numeric doctor id
   from the page URL (`19143`) to an internal UUID.
2. `GET /addresses/api/v1/cities?q={slug}` — resolve the city slug (`kurgan`).
3. `GET /appointments/api/v1/doctors/{uuid}/specialties/{sid}/?cityID=` — offices
   and services of the doctor for the watched specialty. Offices are filtered to
   the target city (a doctor can have offices in several cities).
4. `GET /appointments/api/v1/intervals/doctors?officeID=&doctorID=&serviceID=&slotDate=` —
   availability per date for the next ~30 days.
5. `GET /appointments/api/v1/slots/doctors?...&slotDate=` — concrete times for
   each available date.

### harizma45.ru

harizma45.ru is a Bitrix site without a public JSON API. The only live
availability signal is the server-rendered "ближайший прием: \<date\>" on the
doctor's card of a `/register/...` page (there are no exact times, no 30-day
calendar; when the card shows no date, the doctor has no online slots). The bot
fetches the doctor's booking page each poll and parses that date, so a
notification means "the doctor's next online appointment is now on this date".

The register pages also list every doctor (all specialties) with their current
"ближайший прием", which `--find` uses to search by name.

### cfm.ru

cfm.ru is a WordPress site without a public JSON API. The booking flow is
server-rendered and has four steps:

1. `GET /kurgan/zapis/?step=1` — the directions (services) of the city.
2. `GET /kurgan/zapis/?step=2&service={id}&clinic=0` — the doctors of one
   service with their prices.
3. `GET /kurgan/zapis/?step=2&service={id}&clinic=0&doctor={id}` — the
   doctor's free dates, embedded as a JS array (`freeDates`).
4. `GET /kurgan/zapis/?step=2&…&date={YYYY-MM-DD}` — the free times of one
   date as `step=3` links.

The theme answers these URLs with its 404 page unless the request looks like the
site's own booking iframe, so the bot sends `Sec-Fetch-Dest: iframe` (plus the
matching mode and site headers). Every `<service_id>:<doctor>` pair of
`CFM_DOCTORS` is watched at once; a failure of one doctor does not stop the
other doctors. `--find` walks every service and its doctors, so it finds a doctor
by name without knowing the service id.

New slots (office + service + date + time never seen before) trigger a Telegram
message grouped by office/service. The set of already-notified slots is persisted
in a state file, so restarts never re-notify.

## Configuration (environment variables)

Shared variables use the `SLOTBOT_` prefix. Provider-specific variables use the
`INVITRO_`, `HARIZMA_` or `CFM_` prefix.

| Variable | Default | Description |
|---|---|---|
| `SLOTBOT_TELEGRAM_BOT_TOKEN` | — (required) | Bot token from [@BotFather](https://t.me/BotFather) |
| `SLOTBOT_TELEGRAM_CHAT_ID` | — (required) | Comma-separated chat ids to send notifications to, e.g. `733839622,450097973` |
| `SLOTBOT_DRY_RUN` | `0` | `1` = print notifications to stdout instead of sending |
| `SLOTBOT_PROVIDER` | `invitro` | Clinic to watch: `invitro`, `harizma` or `cfm` |
| `INVITRO_DOCTOR_BITRIX_ID` | `19143` | Numeric doctor id from the Invitro page URL |
| `INVITRO_DOCTOR_NAME` | — | Alternative to `INVITRO_DOCTOR_BITRIX_ID`: find the doctor by name (exact full-name match preferred; ambiguous names are rejected with the candidate list) |
| `INVITRO_CITY_SLUG` | `kurgan` | City slug from the page URL; `--find --city <slug>` overrides it for the search |
| `INVITRO_SPECIALTY_SLUG` | (primary) | Specialty slug to watch, e.g. `ginekolog` |
| `INVITRO_SERVICE_IDS` | (main service) | Comma-separated service UUIDs; if empty, only the doctor's main service (the one used by the "Записаться" flow) is watched — one notification per office |
| `HARIZMA_DOCTOR_ID` | `940` | Internal schedule id of the harizma45.ru doctor |
| `HARIZMA_PAGE_PATH` | `/register/uzi-diagnostika/vozhdaeva-anastasiya/` | Booking page path on harizma45.ru |
| `HARIZMA_DOCTOR_NAME` | — | Alternative to `HARIZMA_DOCTOR_ID`: find the doctor by name on the register page |
| `CFM_DOCTORS` | `3:6164` | Comma-separated `<service_id>:<doctor>` pairs to watch at once, e.g. `3:6164,1089:7188`; the doctor is a numeric id or a name (Коркина and Репин by default are `3:6164` and `1089:7188`) |
| `SLOTBOT_POLL_INTERVAL_SECS` | `120` | Seconds between polls |
| `SLOTBOT_STATE_FILE` | `state.json` | Path of the de-duplication state file |
| `SLOTBOT_NOTIFY_ON_FIRST_RUN` | `1` | `1` = notify about currently open slots on the first poll; `0` = record them as baseline silently |

## Local usage

```sh
cargo build --release

# Run the unit tests (they use a local mock server, no network access):
cargo test

# Inspect what the bot will watch (no Telegram credentials required):
./target/release/slotbot --check

# Find a doctor's booking page by name (default provider: Invitro,
# city from INVITRO_CITY_SLUG):
./target/release/slotbot --find "Хохлова"
# -> Хохлова Ольга Евгеньевна (bitrix_id=19143, ...)
#    booking page: https://www.invitro.ru/kurgan/vrachi/ginekolog/19143

# Search in another city with --city (Invitro only; overrides INVITRO_CITY_SLUG):
./target/release/slotbot --find "Хохлова" --city moscow

# The same commands for МЦ «Харизма»:
SLOTBOT_PROVIDER=harizma ./target/release/slotbot --find "Вождаева"
SLOTBOT_PROVIDER=harizma ./target/release/slotbot --check
# -> Вождаева Анастасия Викторовна (id=940, врач УЗИ-диагностики)
#    booking page: https://harizma45.ru/register/uzi-diagnostika/vozhdaeva-anastasiya/
#    Ближайший прием: ...

# The same commands for ЦСМ в Кургане (cfm.ru):
SLOTBOT_PROVIDER=cfm ./target/release/slotbot --find "Коркина"
SLOTBOT_PROVIDER=cfm ./target/release/slotbot --check
# -> Коркина Ольга Владимировна (id=6164, врач акушер-гинеколог, репродуктолог)
#    service: Акушеры-гинекологи (id=3)
#    booking page: https://www.cfm.ru/kurgan/zapis/?step=2&service=3&clinic=0&doctor=6164

# Find Репин (врач-уролог); --find searches every service, so no ids are needed:
SLOTBOT_PROVIDER=cfm ./target/release/slotbot --find "Репин"
# -> Репин Сергей Сергеевич (id=7188, Врач-уролог)
#    service: Уролог (id=1089)
#    booking page: https://www.cfm.ru/kurgan/zapis/?step=2&service=1089&clinic=0&doctor=7188
# Watch him together with Коркина via CFM_DOCTORS=3:6164,1089:7188.

# Run a single poll cycle (needs the two Telegram vars; use SLOTBOT_DRY_RUN=1 to test):
SLOTBOT_DRY_RUN=1 \
SLOTBOT_TELEGRAM_BOT_TOKEN=123:abc \
SLOTBOT_TELEGRAM_CHAT_ID=456 \
./target/release/slotbot --once

# Watch harizma45.ru for Вождаева:
SLOTBOT_PROVIDER=harizma \
SLOTBOT_TELEGRAM_BOT_TOKEN=123:abc \
SLOTBOT_TELEGRAM_CHAT_ID=456 \
./target/release/slotbot

# Watch cfm.ru (ЦСМ, Курган) for Коркина:
SLOTBOT_PROVIDER=cfm \
SLOTBOT_TELEGRAM_BOT_TOKEN=123:abc \
SLOTBOT_TELEGRAM_CHAT_ID=456 \
./target/release/slotbot

# Run the poll loop:
SLOTBOT_TELEGRAM_BOT_TOKEN=123:abc \
SLOTBOT_TELEGRAM_CHAT_ID=456 \
./target/release/slotbot
```

> macOS note: if the default `cc` in your PATH is not the Apple clang (e.g. a Nix
> clang), linking needs the SDK path:
> `RUSTFLAGS="-C link-arg=-Wl,-syslibroot,$(xcrun --show-sdk-path)" cargo build`
> On Linux/NixOS this is not needed.

## Telegram setup

1. Create a bot with [@BotFather](https://t.me/BotFather) and copy the token.
2. Find your chat id: message your bot, then run
   `scripts/get-chat-id.sh` (prints the most recent chat id) or
   `scripts/get-chat-id.sh --all` (prints all chat ids, one per line).
   For a group chat the id starts with `-100`. Multiple recipients are
   configured as a comma-separated `SLOTBOT_TELEGRAM_CHAT_ID`.

## Deployment on NixOS

The repository is a flake and ships a NixOS module.

```nix
# configuration.nix
{
  inputs.slotbot.url = "github:i-zhirov/slotbot";

  services.slotbot = {
    enable = true;
    package = inputs.slotbot.packages.${pkgs.system}.default;
    environmentFile = "/run/secrets/slotbot.env";
  };
}
```

`/run/secrets/slotbot.env` (mode 600, e.g. via agenix/sops-nix or a manual
systemd credentials file) contains:

```ini
SLOTBOT_TELEGRAM_BOT_TOKEN=123:abc
SLOTBOT_TELEGRAM_CHAT_ID=456
```

Everything else can be overridden through the module options
(`provider`, `doctorBitrixId`, `citySlug`, `specialtySlug`, `serviceIds`,
`harizmaDoctorId`, `harizmaPagePath`, `harizmaDoctorName`, `cfmDoctors`,
`pollIntervalSecs`, `notifyOnFirstRun`).

### Watching several clinics at once

Each module option group maps to one systemd unit, so several clinics can be
watched simultaneously: the top-level options create the default `slotbot`
unit, and every entry of `instances.<name>` creates an extra
`slotbot-<name>` unit with its own state directory (and, optionally, its
own `environmentFile`, `pollIntervalSecs`, …). All clinics can notify the same
Telegram chat or different ones.

```nix
services.slotbot = {
  enable = true;
  package = inputs.slotbot.packages.${pkgs.system}.default;
  environmentFile = "/run/secrets/slotbot.env"; # shared Telegram credentials

  # unit slotbot: Хохлова on Invitro.ru (defaults)
  doctorBitrixId = 19143;

  # unit slotbot-harizma: Вождаева on harizma45.ru
  instances.harizma = {
    provider = "harizma";
    # HARIZMA_DOCTOR_ID / HARIZMA_PAGE_PATH default to Вождаева (id 940)
  };

  # unit slotbot-cfm: Коркина on cfm.ru (ЦСМ, Курган)
  instances.cfm = {
    provider = "cfm";
    # CFM_DOCTORS defaults to Коркина (3:6164)

    # or watch Коркина and Репин (врач-уролог) at once:
    # cfmDoctors = [ "3:6164" "1089:7188" ];
  };
};
```

`journalctl -u slotbot -f` and `journalctl -u slotbot-harizma -f` show
the logs of each instance. Use `enableDefaultInstance = false` to run only the
named instances (e.g. only the harizma one), and `instances.<name>.enable =
false` to keep a configured instance from running.

The services run as dynamic users with a hardened sandbox
(`ProtectSystem=strict`, `NoNewPrivileges`, …), store their state in
`/var/lib/slotbot/state.json` / `/var/lib/slotbot-<name>/state.json`,
restart on failure, and log to journald.

You can also build or run the binary directly from the flake:

```sh
nix build .#default
nix run .# -- --check
```
