# Changelog

## v1.1.0

**In short:** the app got much smaller for most players (one tiny file instead of a ~60 MB download), and for everyone it now uses noticeably less memory and system resources while you play. Ping is measured exactly as before, and your settings are kept.

<details>
<summary>🇺🇦 <b>Коротко українською</b></summary>

Для більшості гравців програма стала набагато меншою (один маленький файл замість завантаження на ~60 МБ), а для всіх вона тепер помітно менше навантажує систему й займає менше пам'яті під час гри. Пінг міряється так само точно, як раніше, налаштування зберігаються.

</details>

<details>
<summary>🇷🇺 <b>Коротко по-русски</b></summary>

Для большинства игроков программа стала намного меньше (один маленький файл вместо загрузки на ~60 МБ), а для всех она теперь заметно меньше нагружает систему и занимает меньше памяти во время игры. Пинг измеряется так же точно, как раньше, настройки сохраняются.

</details>

**Details**

- Rewritten in Rust: a single `AionPingMeter.exe` of about 0.4 MB that needs nothing but Npcap. The separate standalone (~60 MB) and light (needs .NET 8) downloads are gone.
- Memory use while the game runs dropped from 30+ MB to about 8–15 MB; no .NET runtime, no garbage collector, instant start.
- Measurement, alerts, overlay look, tray menu and settings are unchanged; checked side by side with v1.0.2 on live game traffic, pings matched to 0.1 ms.
- New **Open log** tray item while logging is on: opens the log file in Explorer (Windows hides the notification with its path during games).

## v1.0.2

- New overlay look: a compact rounded capsule with signal bars, ping, NET and MAX.
- Alerts now appear as a separate pill under the overlay (red at first, then orange), and the cause reads `ping 1200 server` instead of `ping 1200 (server)`.
- Real per-pixel transparency and smooth rounded corners; the overlay uses the Bahnschrift font built into Windows.

## v1.0.1

- Two downloads: **standalone** (everything built in, nothing to install except Npcap) and **light** (tiny, needs the .NET 8 Desktop Runtime).
- Both are now a single `AionPingMeter.exe` instead of several files.
- Release notes and README explain which one to pick, in English, Ukrainian and Russian.

## v1.0.0

First public release.

- In-game ping measured from the game's own ping exchange, on a single hardware clock (no drift).
- Continuous network round-trip to the game server.
- Lag alerts with a cause label: **server**, **network** or **PC**.
- Server freeze detection (gaps in the server's 20 Hz update stream).
- Packet loss detection (TCP retransmissions on the game connection).
- Click-through overlay, colour-coded tray icon, session counters in the tray tooltip.
- Gateway address in the tray menu (click to copy), optional overlay line.
- Optional CSV log with a per-measurement PC / network / server breakdown.
- Automatic game, adapter and gateway detection; follows relogs, server transfers and VPNs.
