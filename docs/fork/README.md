# Форк agentgateway (`auruhm/agentgateway`) — что это и как с ним работать

Наш форк upstream [`agentgateway/agentgateway`](https://github.com/agentgateway/agentgateway).
**База — релиз `v1.6.0`** (02.10.2026) + коммиты upstream до `c0354ea4`.

Зачем форк: upstream не берёт фичи, решающие «налог промпта» MCP. PR `#3859` (lazy tools)
мейнтейнер отклонил с формулировкой «build on top, not inside» → держим своей веткой и **PR не делаем**.
Решение владельца — `Projects/memory/2026-10-07-курс-на-agentgateway-вместо-toolhive.md`.

## Наши патчи

| Патч | Что даёт | Документ |
|---|---|---|
| `mcp.toolNameOverrides` | декларативная карта переименований MCP-тулов для `prefixMode: never` (иначе коллизии имён теряются) | [`tool-name-overrides-design.md`](tool-name-overrides-design.md) |
| `mcp.lazyTools` | на `tools/list` отдаются только `mcp_search` + `mcp_call` вместо всего каталога; Замер: окно базы — 126 тулов ≈ **63 083** токена → **2 тула ≈ 194** токена (~99.7 %) | [`lazy-bootstrap-report.md`](lazy-bootstrap-report.md) |
| Морфология `mcp_search` | поиск по-русски находит словоформы (FTS5 без морфологии не умеет) | [`mcp-search-ranking.md`](mcp-search-ranking.md) |

Плюс в `feat/all`: **lazy bootstrap без ожидания backend** (`388f55a8`) — бэкенд-задержка больше не
блокирует `initialize`/`tools/list`.

## Ветки

| Ветка | Назначение | HEAD |
|---|---|---|
| `feat/all` | **прод-линия**, отсюда собран рабочий бинарь | `388f55a8` |
| `feat/mcp-lazy-tools` | фича-ветка lazyTools | `9b024fa6` |
| `feat/mcp-search-morphology` | фича-ветка морфологии | `1aa55a3c` |
| `feat/mcp-tool-name-overrides` | фича-ветка rename-map | `22a9f477` |
| `pr/mcp-lazy-tools`, `pr/mcp-tool-name-overrides` | версии под PR | `be8c3d22`, `3a19dd55` |
| `backup/feat-all-pre-rebase-2026-10-13` | состояние `feat/all` до ребейза | `a8d70341` |
| `backup/feat-all-remote-059f3e63` | старая вершина `origin/feat/all` (сборка 08.10) | `059f3e63` |
| `backup/pr-mcp-lazy-tools` | бэкап PR-ветки | `629f57cc` |

Все ветки запушены в `origin` (`https://github.com/auruhm/agentgateway`).

## Текущее состояние (15.10.2026)

- `feat/all` = `388f55a8`, рабочее дерево чистое. Против `upstream/main`: **20 файлов, +3012/−43**.
- **Прод:** `Projects/mcp_platform/.tools/agentgateway/agentgateway-darwin-arm64-patched` —
  release-сборка HEAD (136 МБ). Rollback-копия: `…/backups/2026-10-15-lazy-bootstrap/`.
- Два процесса launchd: `com.local.mcp-agw-comm` (:4488 — память+телеграм, **eager**),
  `com.local.mcp-agw-stack` (:4483–4489 — окна баз + подхаб 1С, **lazyTools**).
- ToolHive (`vmcp-*`) и `mcp-sentinel` **удалены** — их plist'ов нет.
- Тесты: `2314 passed, 1 ignored` (lib), `8/8` bootstrap, `12/12` negotiation.

## Сборка, установка, ребейз

Инструменты: Rust берётся из `rust-toolchain.toml` (1.99.0, rustup ставит сам).
CI GitHub Actions **не используем** — всё локально.

```sh
# сборка и тесты
cargo build --offline --locked -p agentgateway-app --bin agentgateway
cargo test  --offline --locked -p agentgateway --lib
python3 tools/test-lazy-bootstrap.py  target/release/agentgateway
python3 tools/test-lazy-negotiation.py target/release/agentgateway
```

Установка бинаря — **строго в этом порядке** (иначе launchd убивает процесс):

```sh
rm       <целевой>            # cp поверх работающего ломает (OS_REASON_CODESIGNING)
cp       <новый> <целевой>
codesign --force --sign - <целевой>
launchctl bootout gui/$UID/com.local.mcp-agw-comm ; launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.local.mcp-agw-comm.plist
# то же для com.local.mcp-agw-stack
```

Перед заменой сохранить предыдущий бинарь в `mcp_platform/.tools/agentgateway/backups/<дата>/`.

**Ребейз на новый upstream:** три патча накладываются на `upstream/main` одним диффом,
**исключая** генерируемые `schema/config.json` и `schema/config.md` (их затем перегенерировать
`cargo xtask schema`). `git apply --3way` ранее слил без конфликтов; контроль — `git diff --stat`
должен дать те же ~1026 insertions / 31 deletion.

## Upstream (проверено 15.10.2026)

- Последний релиз — **`v1.6.0`** (02.10). Мы на нём, нового релиза нет.
- `upstream/main` впереди на **59 коммитов** (все от 15.10), часть из них трогает
  `crates/agentgateway/src/types/local.rs` — один из наших файлов, так что ребейз потребует внимания.
  Срочности нет: наша база — действующий релиз.
- PR **`#3838`** (toolNameOverrides) — open. Issue **`#963`** — open.
  PR **`#3859`** (lazy tools) — отклонён, повторно не подаём.

## Открытые пункты

1. Прогнать `tools/test-lazy-bootstrap.py` на **release**-артефакте — валидировали debug-кандидат.
2. Живая UI-проверка lazy-пути в Zed (не скриптуется, делается руками).
3. Ребейз на 59 коммитов upstream — когда накопится потребность.
4. `#3838` — только следить за статусом.

## Источники

- Решения и хвосты: `Projects/memory/2026-10-07-курс-на-agentgateway-вместо-toolhive.md`,
  `2026-10-08-agentgateway-lazy-tools.md`, `2026-10-08-agentgateway-отказы-и-фасад.md`,
  `2026-10-11-бриф-холстарт-форк-морфология.md`, `2026-10-13-обновление-софта-форки-и-вендор.md`.
- Статус миграции и живая проба: `Projects/mcp_platform/research/AGENTGATEWAY_MIGRATION_STATUS_2026-10-07.md`,
  `…/AGENTGATEWAY_POC_2026-10-07.md`.
- Стек и роадмап: `Projects/ai_tools/multi-har/ROADMAP.md`.
