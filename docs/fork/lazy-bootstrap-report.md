# Локальный bootstrap lazyTools — проверка candidate

> **Статус на 15.10.2026: исправление развёрнуто в проде.** Из `feat/all` (`388f55a8`) собрана
> release-сборка и установлена в `mcp_platform/.tools/agentgateway/agentgateway-darwin-arm64-patched`
> (rollback — в `…/backups/2026-10-15-lazy-bootstrap/`), оба процесса перезапущены.
> Общая карта форка — [`README.md`](README.md). Раздел «Что нужно для prod» ниже отражает
> состояние **на момент проверки debug-кандидата**, а не текущее.
>
> **На момент проверки** (debug-кандидат, Production ещё не переключён): после исправления
> stack-boundary полная lib suite: **2314 passed, 0 failed, 1 existing ignored**. Бинарные
> интеграции: **8/8 bootstrap + 12/12 negotiation PASS**, включая default-stack stateless regression,
> ping-before-init, версии/транспорты/auth и отсутствие replay.

## Финальная проверка Main

- `cargo build --offline --locked -p agentgateway-app --bin agentgateway`: PASS, 2 мин 19 с.
- `python3 tools/test-lazy-bootstrap.py target/debug/agentgateway`: 8/8 PASS. Backend delay в fixture не блокирует initialize/list; ZERO ранних backend requests подтверждены.
- `python3 tools/test-lazy-negotiation.py target/debug/agentgateway`: 12/12 PASS.
- `cargo test --offline --locked -p agentgateway --lib -- --test-threads=2`: 2314 PASS, 1 существующий ignored; 0 failed. Использован прямой установленный rustc/cargo 1.99.0, jobs=1, без rustup sync.
- Проверенный debug executable: /Users/AU_1/Projects/agentgateway-fork/target/debug/agentgateway, SHA256 de34b32df9882a3d69cf28da3492325bd08742f5691d1a3ce6dedda15a2b3c3b.
- По последнему bootstrap-прогону: legacy initialize 2,0–8,33 мс, tools/list 0,61–1,48 мс; modern discover 1,91–11,99 мс, tools/list 1,02–1,86 мс. Это локальные тесты, не обещание production latency.
- RSS тестовых gateway около 66–69 МБ; CPU sample 0–22% на коротком startup/request окне, не idle benchmark. Все тестовые процессы завершены.
- Release/LTO ранее дважды превысил 600 с. Новый optimized production artifact не получен; debug-кандидат не устанавливался в production. Работающие comm/stack binary/config/порты не менялись. До production activation нужны оптимизированная сборка, проверка того же binary и короткий restart с возможностью отката.

Ниже сохранена история диагностики; указания «не собран» относятся к соответствующему промежуточному состоянию, не к финальному debug-кандидату.

## История: Stateless search / worker stack до финальной сборки

Main сообщил GREEN полного lib suite (2313 passed, 1 ignored), исправление `EmptyObject {}` и GREEN debug app build (3м18с). Этот новый debug binary использован ниже как baseline; исправление Main сохранено.

Падающий третий сценарий `test-lazy-bootstrap.py` — **legacy stateless (false, 2025-06-18, search-first)**, не modern stateful. Отдельный modern stateful сценарий прошёл с правильным `server/discover -> tools/list -> server/discover -> tools/call`; fixture modern discovery не устарела.

Диагностика без cargo/release:

- Стандартный worker stack: legacy stateless search не доходит до первого backend request и превышает исходный 2-секундный HTTP deadline. Call-first в том же режиме проходит.
- CPU тестового процесса после deadline 55–57%, RSS 54 944 КиБ; это не медленный backend. `sample` показывает глубокий inline poll stack `serve_stateless -> Session.send -> lazy search/fanout -> ready/negotiate/exchange -> HTTP client/pool`.
- Контрольный диагностический запуск ТОГО ЖЕ binary/сценария с 8-МиБ stack, без изменения 2-секундного deadline, проходит за миллисекунды. Это stack-depth-sensitive stall; точный аппаратный механизм переполнения по sample не утверждается.
- Текущий debug baseline прошёл все прежние 11 negotiation scenarios, включая modern native/fallback, HTTP/SSE/stdio ping, auth denial и side-effect error без replay.

Исправление: `LazyUpstream::ready(self: &Arc<Self>)` запускает только deferred negotiation в отдельной Tokio task под прежним OnceCell. Это разрывает глубокую inline polling-chain, не увеличивает production stack/timeout и не переносит реальный tools/call в retry. Tracing span наследуется. `await_handshake` использует abort-on-drop guard: отмена waiter отменяет task, не оставляет detached handshake.

Регрессии:

- `tools/test-lazy-negotiation.py`: новый `test_stateless_search_with_deferred_handshake_on_default_stack` фиксирует 2-МиБ worker stack, прежний 2-секундный deadline, ZERO backend requests на bootstrap и точный init/initialized/list порядок при search. **RED подтверждён на baseline binary**.
- `tools/test-lazy-bootstrap.py` также фиксирует обычный 2-МиБ worker stack, чтобы большой inherited `RUST_MIN_STACK` не скрывал регрессию. На ошибке печатает точные stateful/version/call_first и backend methods. Assertions/timeout не ослаблялись.
- Rust unit test `handshake_waiter_cancellation_aborts_worker` проверяет освобождение ресурсов worker после отмены waiter.

Новый Rust патч НЕ собирался, GREEN после него ещё нет. После освобождения Main build slot нужны debug build и:

```sh
python3 tools/test-lazy-negotiation.py target/debug/agentgateway NegotiationTests.test_stateless_search_with_deferred_handshake_on_default_stack
python3 tools/test-lazy-bootstrap.py target/debug/agentgateway
python3 tools/test-lazy-negotiation.py target/debug/agentgateway
```

Затем Rust test `handshake_waiter_cancellation_aborts_worker` и полный lib suite. Всего negotiation Python scenarios теперь 12. Логи: `target/lazy-bootstrap-hang-trace.log`, `target/lazy-modern-hang-baseline.log`, `target/lazy-stateless-hang.sample.txt`, `target/lazy-stateless-search-resources.log`, `target/lazy-stateless-search-stack-diagnostic.log`, `target/lazy-negotiation-current-baseline.log`, `target/lazy-default-stack-regression-red.log`.

## P2 retention — исторический статус исходников до сборки Main

Независимый review подтвердил закрытие двух P1. Оставшийся P2 исправлен без cargo/release/prod:

- `Process` сохраняет service context только при первом внутреннем initialize; последующие user requests не обновляют его.
- `IncomingRequestContext::for_service_reply()` строит новый POST context без body и исходного URI/query. Удалены framing, MCP routing/argument headers, trace headers; extensions копируются только по явному allowlist: validated JWT/API-key/Basic claims, ext-authz auth metadata, source/actor/egress identity, auto-hostname и backend timeout. Log/span/MCP/request snapshot extensions не удерживаются.
- SSE сохраняет handshake headers, поскольку credentials и policy expressions могут читать произвольные custom headers/cookies. Это осознанное удержание handshake auth material, не last tool-call payload. Исходная authority сохраняется для прежней HTTP routing semantics.
- `TokioChildProcess` переопределяет transport hook и возвращает полностью пустой контекст: stdio не нуждается в HTTP headers/credentials/extensions.
- Тесты `service_reply_context_does_not_retain_body_or_request_extensions` и `service_pongs_keep_handshake_context_not_last_user_request`: проверяют освобождение 1-МиБ body/extension через Weak при живом idle transport, сохранение auth/timeout, отсутствие user routing/arguments и использование handshake credentials для pong при следующих запросах с другими headers/body. Настоящий tool call сохраняет свой полный policy context.

Лёгкие проверки P2: rustfmt успешно разобрал и отформатировал оба Rust-файла, `git diff --check` чисто. Новые Rust-тесты **не компилировались/не запускались**: Main должен выполнить их и предыдущую negotiation/ping матрицу после debug build. Старый candidate не пересобирался и остаётся заблокированным.

## Исправления P1 — исторический статус до подтверждения review

Heavy cargo, release и rustup в этом проходе не запускались: Main собирает native Zed отдельно.

- `upstream/lazy.rs`: отдельная negotiation на target. Modern Streamable HTTP сначала выполняет deferred `server/discover`; только протокольный отказ разрешает fallback на legacy initialize. Auth 403 и прочие ошибки не становятся fallback. Для legacy принимается известная поддерживаемая backend версия, а не только точное совпадение с downstream.
- Перед реальным запросом backend получает свою negotiated HTTP version и соответствующую `_meta`; auth/context и пользовательская metadata сохраняются. Обязательные modern поля синтезируются для gateway либо удаляются для legacy. Legacy MRTR `inputResponses/requestState` отклоняются до отправки tools/call. Ответы клиенту проходят существующую downstream-нормализацию resultType/cache fields. Это tools-мост, а не заявление о поддержке всех optional extensions.
- HTTP handshake отвечает на server ping с исходным id до initialize result. SSE/stdio receive-loop обслуживает внутренний handshake по непередаваемому маркеру и продолжает обслуживать gateway-owned callbacks без downstream GET. Nonlazy не получает этот маркер.
- Реальный tools/call не используется для определения протокола и не повторяется. Negotiated context применяется также к notifications, GET, ответам и cleanup DELETE.
- Rust tests расширены divergent 2025-06-18 -> 2025-03-26 и modern -> legacy; добавлены тесты адаптации/auth/meta/MRTR/fallback и fake transport, который не отдаёт init result до pong.
- `tools/test-lazy-negotiation.py`: 11 изолированных сценариев, включая HTTP/SSE/настоящий stdio ping-before-init, divergent versions, modern->legacy, native modern, auth denial и ошибку side-effecting call без replay. `tools/test-lazy-bootstrap.py` обновлён под deferred modern discovery перед каждым реальным stateless запросом.

### Что реально проверено в review-проходе

Прежний debug candidate использован только как RED baseline, не как проверка новых исходников:

```sh
python3 tools/test-lazy-negotiation.py target/candidates/agentgateway-3fbe9e4d-lazy-tools-debug NegotiationTests.test_ping_http NegotiationTests.test_divergent_legacy_versions NegotiationTests.test_modern_to_legacy_http
python3 tools/test-lazy-negotiation.py target/candidates/agentgateway-3fbe9e4d-lazy-tools-debug NegotiationTests.test_ping_sse NegotiationTests.test_ping_stdio
```

Получены ожидаемые RED: HTTP/SSE/stdio ждут pong и завершаются по timeout; divergent legacy возвращает 500 с `upstream protocol ... differs`; modern->legacy посылает semantic request до initialized с неверными header/meta/session. Логи: `target/lazy-review-red.log`, `target/lazy-review-ping-transports-red.log`. Процессы этих тестов завершены.

**Новый Rust код не компилировался и не прошёл GREEN в этом проходе.** Rustfmt/проверка Python syntax/diff — только лёгкие проверки, не замена typecheck и runtime.

После освобождения Main build slot нужны debug build/tests (не release): фильтры `lazy_`, `deferred_initialize_services_ping_without_downstream_event_stream`, `mcp::upstream::lazy::tests`, затем полный MCP/lib suite и оба Python-скрипта на НОВОМ binary. До этого оба P1 не считать закрытыми доказательно.

---

## Исторический отчёт до независимого review — НЕ текущая приёмка

Дата: 2026-10-15. База: `3fbe9e4d`, ветка `feat/all`. Коммитов, переключений веток и push не было.
Изменения ограничены этим репозиторием; prod binary, конфиги `mcp_platform` и порты 4483–4488 не менялись.

## Реализация

- `session.rs`: сохраняет отфильтрованный initialize для deferred handshake; stateless legacy bootstrap не пытается маршрутизировать gateway-generated `mcp_search`/`mcp_call` как backend-инструменты.
- `handler.rs`: локальные lazy initialize, server/discover, tools/list и legacy ping. Сохраняется прохождение request/response guardrails. Внутренний tools/list поиска не помечается локальным: он действительно читает backend.
- `upstream/mod.rs`: lazy transport создаётся только для реального запроса, в том числе stdio process. Handshake сериализован через OnceCell; initialize result и версия проверяются до initialized notification и до инструментального вызова. Нет автоматического replay tools/call.
- `mergestream.rs`: event stream может дождаться deferred transport, не открывая соединение на bootstrap/GET.
- Nonlazy сохраняет прежние fanout, transport factory, policy client и маршрутизацию. Backend auth/HTTP policies/лимиты остаются в существующем PolicyClient; authorization реальных инструментов остаётся в существующем Session path.

## Candidate

Проверенный **debug, не оптимизированный** бинарник:

`/Users/AU_1/Projects/agentgateway-fork/target/candidates/agentgateway-3fbe9e4d-lazy-tools-debug`

SHA-256:

`4fb89d88f48977b61f0afe29dc5deb4116270bdc1163afd2b0fb55603c6a97af`

Это staged-артефакт в каталоге candidates, **не git stage**. Старый `target/release/agentgateway` НЕ является результатом этой работы.

## Точные команды

Рабочий каталог, если не указан другой: `/Users/AU_1/Projects/agentgateway-fork`.
Установленный toolchain используется напрямую; rustup sync и установка ПО не выполнялись.
Каждая сборка ограничивалась 600 секундами, jobs=1, offline/locked.

```sh
env RUSTC=/Users/AU_1/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/rustc CARGO_BUILD_JOBS=1 /Users/AU_1/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo test --offline --locked -p agentgateway --lib lazy_
env RUSTC=/Users/AU_1/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/rustc CARGO_BUILD_JOBS=1 /Users/AU_1/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo build --offline --locked -p agentgateway-app --bin agentgateway
python3 tools/test-lazy-bootstrap.py target/candidates/agentgateway-3fbe9e4d-lazy-tools-debug
git --no-pager diff --check
```

Полный lib suite, рабочий каталог `/Users/AU_1/Projects/agentgateway-fork/crates/agentgateway`:

```sh
/usr/bin/time -l env TMPDIR=/tmp INSTA_WORKSPACE_ROOT=/Users/AU_1/Projects/agentgateway-fork ../../target/debug/deps/agentgateway-204edbdd96523f21 --test-threads=2
```

## Результаты

- RED до исправления: `lazy_bootstrap_does_not_contact_slow_backend` упал с `lazy bootstrap waited for backend: Elapsed(())` на deadline 500 мс; backend задерживал ответ на 5 секунд.
- GREEN: три теста по фильтру `lazy_`, включая два новых regression tests. Bootstrap проверяется с медленными HTTP/SSE targets и несуществующим stdio executable: backend requests = 0, stdio не запускается.
- Матрица deferred handshake: 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25 и 2026-07-28, оба значения backend stateful. Проверяются порядок initialize/initialized/list/call и отсутствие replay.
- MCP subset: **372 passed**, 0 failed.
- Полный lib suite на окончательном коде: **2306 passed, 0 failed, 1 ignored**. Existing ignored test не менялся. 8,06 с real, 4,02 с user, 0,65 с sys; max RSS 255 934 464 байта (~244 МиБ).
- Изолированная бинарная интеграция: **8/8 сценариев**: legacy 2025-06-18 / modern 2026-07-28 × stateful/stateless backend × search-then-call/call-first. Все служебные порты случайные loopback, без prod портов.
- Строгий backend проверяет завершение handshake и передачу upstream session ID до legacy list/call. Ровно один tools/call; call-first не вызывает tools/list при однозначной маршрутизации.
- Последний запуск именно staged candidate: initialize 2,24–7,86 мс, tools/list 1,12–1,49 мс, modern server/discover 1,82–2,92 мс. До discovery/call — ZERO backend requests. Это локальный функциональный замер debug binary, не нагрузочный benchmark.
- Снимки `ps` gateway: CPU 0–10,6%, RSS 67 264–69 472 КиБ (~66–68 МиБ). Это короткие startup/request samples, не idle CPU и не peak RSS.
- Все тестовые gateway/server процессы завершены. Проверка процессов после tests оставила только существовавшие prod gateway; они не останавливались.
- `diff --check`: чисто.

## Неудачные попытки и ограничения проверки

- Два прохода debug test compilation завершились по timeout 600 с; последующие проходы из кэша завершились успешно.
- Первые запуски полного harness имели неверный cwd/insta workspace root и длинный macOS TMPDIR: missing fixtures, неверные snapshot paths и `path must be shorter than SUN_LEN`. Исправлено окружение запуска, не код тестов; итоговый полный запуск зелёный. Созданные этими попытками `.snap.new` удалены, существующие snapshots не менялись.
- Первый вариант Python fixture использовал неверные поля конфигурации (`stateful` вместо `statefulMode`, host+path без port); fixture исправлена по реальному parser. Modern ping правильно возвращает 404, поэтому проверяется только legacy ping.
- Одна попытка запуска свежего binary превысила startup readiness deadline без логов; точная причина не установлена. Deadline старта fixture увеличен с 5 до 15 с отдельно от 500-мс bootstrap assertion; последующие запуски зелёные.
- Release build с LTO дважды превысил 600 с. Библиотека скомпилировалась, приложение/release artifact не завершено; фоновых компиляторов не осталось. Логи: `target/lazy-release-build.log`, `target/lazy-release-build-final.log`.
- Debug incremental cache, созданный этой сессией, удалён ради диска. Candidate, test executable, dependencies и логи сохранены; свободно около 17 ГиБ после очистки.
- Новые strict wire-level integration tests покрывают Streamable HTTP. Для SSE/stdio доказан deferred bootstrap; их полный новый end-to-end handshake отдельными внешними серверами не прогонялся.

## Протокольные границы

1. Локально объявляются только известные gateway tools capabilities. Не угадываются upstream extensions, prompts/resources/tasks/subscriptions/listChanged, не переносятся неизвестные инструкции backend. Gateway identity/instructions overrides сохраняются. Это наблюдаемое отличие lazyTools от eager bootstrap; nonlazy не меняется.
2. Legacy backend должен подтвердить версию, выбранную локальным initialize. При несовпадении discovery/call завершается ошибкой **до tools/call**: протокольной трансляции и скрытого повтора нет. Заранее выбрать минимум версий backend без контакта с ним невозможно.
3. Modern запросы не получают synthetic legacy initialize: сохраняется существующая граница протокола. Backend должен принимать modern per-request envelope; legacy-only backend под modern клиентом не получает автоматического downgrade/replay.
4. Stateless legacy каждый реальный HTTP-запрос создаёт свой upstream handshake и cleanup. Stateful handshake однократный на target/session, не на каждый search/call.
5. Lazy downstream sessions удерживаются в памяти SessionManager; при bootstrap нет полного набора backend session state для persisted resume после рестарта. Рестарт требует client reconnect/initialize.
6. Внутренний lazy search сохраняет существующие каталог/ранжирование/RBAC/failureMode; не добавлялись новые кэши, retry или replay side effects.

## Что нужно для prod

- Согласовать вышеописанные lazy capability/version границы с фактическими клиентами/backend и проверить совпадение legacy версий.
- Получить оптимизированный release artifact в подходящем CI/окне сборки; debug candidate не использовать как production performance artifact.
- На release artifact повторить `tools/test-lazy-bootstrap.py`, затем отдельную проверку фактических backend с auth/policies и нужными legacy/modern версиями.
- Только отдельным разрешённым действием: сохранить текущий prod binary для rollback, заменить активируемый artifact, перезапустить нужный hub, переподключить клиентов и проверить latency/CPU/RSS. В этой задаче эти действия НЕ выполнялись.

Логи проверки: `target/lazy-tests.log`, `target/mcp-suite.log`, `target/lazy-full-suite-final.log`, `target/lazy-build.log`, `target/candidates/lazy-integration.jsonl`.
