# Agentgateway: дизайн declarative tool rename map

Дата аудита: 2026-10-07  
Репозиторий: [`agentgateway/agentgateway`](https://github.com/agentgateway/agentgateway)  
Проверенный срез `main`: [`7f23e9881716700fb7c75edeba3fb5645604d086`](https://github.com/agentgateway/agentgateway/commit/7f23e9881716700fb7c75edeba3fb5645604d086), 2026-10-06  
Последний релиз на дату аудита: [`v1.6.0`](https://github.com/agentgateway/agentgateway/releases/tag/v1.6.0), 2026-10-02

## Резюме

**Вердикт: небольшой upstreamable patch реалистичен.** Наиболее узкий вариант — расширить только семантику `prefixMode: never` декларативным backend-level списком переименований tools. Производственный код оценивается в **160–290 LOC**, тесты и документация — ещё **230–430 LOC**, итого **390–720 LOC**. Изменения затрагивают 4 основных Rust-файла, 2 тестовых файла, generated schema/docs и один пример.

Желаемое поведение естественно встраивается в существующую архитектуру:

- `tools/list` уже объединяет результаты по target, удаляет неоднозначные bare names и переписывает имена;
- `tools/call` при `prefixMode: never` уже выполняет обратный lookup владельца через внутренние `tools/list` запросы;
- перед отправкой upstream запрос уже приведён к native namespace, после чего запускаются `mcpGuardrails`;
- standalone-конфигурация нормализуется в immutable backend snapshot и заменяется штатным hot reload;
- `failureMode: failOpen` реализован на fanout/инициализации upstream и не требует изменений.

Рекомендуемый patch:

1. Добавить `toolNameOverrides` в MCP backend, только для `prefixMode: never`.
2. При нормализации собрать два индекса:
   - `(target, source) → public` для `tools/list`;
   - `public → (target, source)` для `tools/call`.
3. В `tools/list` сначала применить явные overrides, затем проверять конфликты уже в публичном namespace.
4. В `tools/call` сначала искать имя в explicit reverse index; при отсутствии — использовать нынешний bare-name resolver.
5. До `generic_stream` заменить публичное имя на исходное. Тогда transport, telemetry и request-phase guardrails продолжат видеть native имя.

**Внешний `mcpGuardrails`/extMcp plugin не является полноценной альтернативой:** single-target guardrail запускается после выбора target и видит уже unmuxed имя. Он может мутировать params/result, но не может надёжно выполнить начальный `public → target` routing. Отдельный MCP adapter-процесс возможен, но фактически должен повторить multiplexing, pagination, authorization-sensitive discovery, fail-open и hot reload; это больше кода и ещё один runtime hop.

## Границы и методика

Аудит выполнен без `git clone`, checkout, build и установки ПО. Использовались только GitHub API, GitHub raw/blob и точечное чтение исходников. Номера строк ниже закреплены за SHA `7f23e988…`, а не за подвижным `main`.

Не входят в предлагаемый patch:

- переименование prompts/resources;
- изменение аргументов, descriptions и filtering из общего feature request [#963](https://github.com/agentgateway/agentgateway/issues/963);
- Kubernetes/Gateway API/xDS/CRD surface;
- новый persistent discovery cache;
- изменение поведения `prefixMode: conditional` и `always`;
- virtual consolidation нескольких одинаковых tools из [#3382](https://github.com/agentgateway/agentgateway/issues/3382).

## Текущее устройство

### Конфигурационные types и parsing

Основной runtime type — `McpBackend`: targets, `prefix_mode`, `failure_mode`, session settings и server overrides находятся в [`crates/agentgateway/src/types/agent.rs#L1895-L1952`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/agent.rs#L1895-L1952). `McpPrefixMode::Never` документирован как bare namespace с lookup владельца и требованием уникальности names в [`agent.rs#L1895-L1907`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/agent.rs#L1895-L1907).

Target задаётся `McpTarget { name, condition, spec }`; target name валидируется по Gateway API `SectionName`, а `_` и `+` зарезервированы как MCP delimiters: [`agent.rs#L1988-L2025`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/agent.rs#L1988-L2025).

Standalone YAML type `LocalMcpBackend` содержит `targets`, `statefulMode`, `prefixMode`, `failureMode` и остальные MCP options: [`crates/agentgateway/src/types/local.rs#L1903-L1945`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/local.rs#L1903-L1945). Нормализация targets и сборка `McpBackend` происходят в [`local.rs#L1746-L1852`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/local.rs#L1746-L1852). Это правильное место для статической валидации map и компиляции индексов.

Отдельный ручной YAML parser для поля не нужен: `parse_config` выполняет shell expansion и deserializes `NestedRawConfig` через serde в [`crates/agentgateway/src/config.rs#L33-L42`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/config.rs#L33-L42). Derive-макросы типов формируют [`schema/config.json`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/schema/config.json) и [`schema/config.md`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/schema/config.md).

### `tools/list`: merge, rewrite и конфликты

В [`crates/agentgateway/src/mcp/handler.rs#L69-L124`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/handler.rs#L69-L124):

- `resource_name` строит prefixed либо bare public name;
- `duplicate_names` вычисляет дубликаты;
- `per_target_deduped` сохраняет provenance `(target, items)` и удаляет неоднозначные names.

`Relay::merge_tools` включает duplicate rejection именно когда требуется unprefixed resolution, затем применяет authorization и public-name rewrite: [`handler.rs#L796-L835`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/handler.rs#L796-L835).

Следствие: provenance target ещё доступен в точке merge, поэтому rename не требует менять transport или MCP model. Достаточно заменить текущую функцию `resource_name(prefix_names, target, source)` на tool-specific resolver, а дедупликацию выполнять по итоговому public name.

### `tools/call`: выбор target и native name

При `prefixMode: never` `resolve_resource_name` вызывает `resolve_unprefixed`; при остальных режимах используется существующий parser target prefix: [`handler.rs#L430-L460`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/handler.rs#L430-L460).

`resolve_unprefixed` опрашивает каждый доступный target внутренним paginated `tools/list`/`prompts/list`, сохраняет исходный `_meta`, пропускает недоступный target при `failOpen` и отклоняет имя при нуле либо нескольких владельцах: [`handler.rs#L274-L318`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/handler.rs#L274-L318), [`handler.rs#L462-L525`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/handler.rs#L462-L525).

После resolution request отправляется выбранному upstream через `generic_stream`. Там telemetry и `mcpGuardrails` извлекают tool name непосредственно из `CallToolRequest.params.name`: [`crates/agentgateway/src/mcp/upstream/mod.rs#L304-L368`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/upstream/mod.rs#L304-L368).

Отсюда критический ordering-контракт: reverse mapping и замена `params.name` должны завершиться **до** `generic_stream`. Тогда upstream, telemetry, backend policies и single-target guardrails видят исходное имя, как и сегодня.

### Runtime plumbing

`router.rs` переводит `McpBackend` в request-local `McpBackendGroup`, копируя `prefix_mode`, `failure_mode` и server settings: [`crates/agentgateway/src/mcp/router.rs#L96-L118`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/router.rs#L96-L118). Сам `McpBackendGroup` объявлен в [`router.rs#L242-L265`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/router.rs#L242-L265). Скомпилированные rename indexes нужно провести тем же путём.

### `failOpen`

Текущее разрешение bare name уже обрабатывает ошибку внутреннего list probe по `failure_mode`: при `FailOpen` target пропускается, иначе ошибка возвращается клиенту — [`handler.rs#L482-L505`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/handler.rs#L482-L505). Fanout использует тот же `failure_mode`; менять эту механику не нужно.

Explicit reverse mapping не должно проверять живость target отдельным глобальным cache. Оно выбирает configured target и source, после чего существующая инициализация/отправка формирует штатную ошибку для недоступного выбранного tool. `failOpen` при этом означает, что отказ одного target не ломает listing и работу tools остальных targets, а не что вызов заведомо недоступного tool должен успешно завершиться.

### Hot reload

Standalone watcher реагирует на изменение файла и вызывает reload: [`crates/agentgateway/src/state_manager.rs#L138-L174`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/state_manager.rs#L138-L174). Reload заново выполняет `NormalizedLocalConfig::from`, синхронизирует binds/backends и фиксирует успех: [`state_manager.rs#L247-L295`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/state_manager.rs#L247-L295). При ошибке возвращается прежний `PreviousState`, а gateway продолжает обслуживать last-good config: [`state_manager.rs#L307-L324`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/state_manager.rs#L307-L324).

Поэтому hot reload сохраняется автоматически, если map является частью `LocalMcpBackend → McpBackend → McpBackendGroup`, а не отдельным mutable singleton/cache.

## Предлагаемый YAML-контракт

### Форма

```yaml
mcp:
  prefixMode: never
  failureMode: failOpen
  toolNameOverrides:
  - target: code-metadata
    source: search_forms
    name: code_metadata_search_forms
  - target: graph
    source: search_forms
    name: graph_search_forms
  targets:
  - name: code-metadata
    mcp:
      host: http://code-metadata:8080/mcp
  - name: graph
    mcp:
      host: http://graph:8080/mcp
  - name: syntax
    mcp:
      host: http://syntax:8080/mcp
```

Результат:

- `code-metadata/search_forms` публикуется как `code_metadata_search_forms`;
- `graph/search_forms` публикуется как `graph_search_forms`;
- уникальный `syntax/check_file` остаётся bare `check_file`;
- вызов `graph_search_forms` детерминированно превращается в `(target=graph, source=search_forms)`;
- вызов `check_file` использует существующий dynamic bare-name lookup.

### Почему список на backend, а не nested map или поле target

Рекомендуется **backend-level list объектов**:

- явно выражает тройку `public → (target, source)`;
- хорошо документируется JSON Schema;
- не использует dynamic YAML keys;
- позволяет одной валидации увидеть весь публичный namespace;
- соответствует терминам `source`/`name` из открытого issue [#963](https://github.com/agentgateway/agentgateway/issues/963), облегчая последующее объединение с более широкой `toolCustomizations` feature.

Поле внутри каждого target (`toolNameOverrides: {source: public}`) короче, но скрывает reverse contract и усложняет глобальную проверку duplicate public names. Backend map вида `public: {target, source}` лучше runtime-направлению, но хуже читается при ревью и расходится с уже предложенным upstream vocabulary.

### Ограничения контракта

1. `toolNameOverrides` допустим только при `prefixMode: never`. Для `conditional`/`always` конфигурация отклоняется, чтобы не вводить второй порядок prefix-vs-rename.
2. `target`, `source`, `name` обязательны и непусты.
3. `target` обязан существовать в `mcp.targets`.
4. `(target, source)` встречается не более одного раза.
5. `name` уникально среди всех overrides.
6. Identity entry (`source == name`) разрешать не следует: она не добавляет смысла и маскирует ошибку конфигурации.
7. Override применяется стабильно, даже когда фактического конфликта сейчас нет или второй target временно недоступен. Иначе public API меняется от здоровья upstream.
8. Конфликт итоговых public names, обнаружимый только после discovery, не должен silently drop tools. `tools/list` обязан вернуть понятную ошибку namespace collision с перечислением `(target, source)`; это ошибка конфигурации/динамического каталога, а не transport failure.
9. Если configured `source` отсутствует в текущем list target, listing просто не содержит его. Прямой вызов public name всё равно маршрутизируется в configured target/source и получает штатный upstream `unknown tool` либо transport error. Это сохраняет детерминированность и не требует stale discovery cache.
10. Если один raw name встречается у нескольких активных targets, допустимы два режима policy. Для первого PR рекомендуется строгий: **каждое конфликтующее вхождение должно иметь override**, иначе `tools/list` возвращает configuration collision. Это устраняет нынешнее молчаливое исчезновение ambiguous tools и делает намерение оператора явным.

Последний пункт стоит согласовать в issue до реализации. Более permissive вариант — разрешить переименовать только одну сторону, после чего вторая становится уникальной и остаётся bare. Он меньше по коду, но public namespace сильнее зависит от неполной конфигурации.

## Runtime model и алгоритм

### Types

Предлагаемые логические types (имена ориентировочные):

```rust
pub struct McpToolNameOverride {
    pub target: McpTargetName,
    pub source: Strng,
    pub name: Strng,
}

pub struct McpToolNameOverrides {
    by_source: HashMap<(McpTargetName, Strng), Strng>,
    by_public: HashMap<Strng, (McpTargetName, Strng)>,
}
```

Публичный serde/schema type лучше хранить как `Vec<McpToolNameOverride>`, runtime indexes — как отдельный compiled type либо private fields. Не строить глобальный индекс из текущего `tools/list`: targets условны, списки paginated, availability меняется, а reload уже даёт корректную immutable boundary.

### `tools/list`

Для каждого результата сохранить `(target, source, Tool)` и выполнить:

1. Authorization/RBAC остаётся привязанным к native `target + source`, как сейчас.
2. Найти `(target, source)` в `by_source`.
3. Если найдено — назначить explicit public `name`; иначе оставить bare source.
4. Проверить уникальность всех полученных public names на этой логической выдаче.
5. Для raw collisions применить согласованную strict policy: все вхождения должны быть overridden.
6. Сохранить pagination cursor без изменений.
7. Вернуть tools в текущем порядке, меняя только `Tool.name`.

Важно: collision validation должна учитывать pagination. Нынешний resolver умеет проходить последующие страницы, и тест уже фиксирует это в [`mcp_tests.rs#L795-L813`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/mcp_tests.rs#L795-L813). Нельзя считать namespace безопасным только по первой странице. Минимально upstreamable поведение — сохранить нынешнюю page-local выдачу, а гарантировать детерминированный call через explicit map; строгую глобальную проверку всех динамических collisions можно вынести из первого PR. В документации следует честно указать эту границу.

### `tools/call`

1. Получить client-facing `params.name`.
2. Проверить `by_public`:
   - hit: получить `(target, source)` без discovery probe;
   - miss: вызвать существующий `resolve_resource_name(ResolveKind::Tool, public, …)`.
3. При explicit hit найти target среди уже отфильтрованных/инициализированных request-local upstreams. Нельзя обходить target `condition` или authorization context.
4. Заменить `params.name = source`.
5. Продолжить текущий путь через `generic_stream`.

Такой ordering сохраняет заявленный guardrails contract: single-target methods видны процессорам в native namespace. Этот contract прямо описан в [`crates/agentgateway/src/mcp/guardrails/mod.rs#L1-L10`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/guardrails/mod.rs#L1-L10).

### Неизменяемые свойства

- `_meta` исходного call и внутренних list probes сохраняется.
- Arguments не меняются.
- Request ID, tasks, notifications и subscriptions не меняются.
- RBAC и target conditions не обходятся.
- `failureMode` не интерпретируется новым кодом, кроме использования существующего пути.
- При отсутствии `toolNameOverrides` поведение bit-for-bit совместимо с текущим.

## Точные точки изменения

| Файл | Изменение | Оценка production LOC | Риск конфликта |
|---|---|---:|---|
| [`types/agent.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/agent.rs#L1895-L2025) | schema/runtime types, field в `McpBackend`, validation helper | 35–65 | низкий/средний |
| [`types/local.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/local.rs#L1746-L1945) | YAML field, target/source/public validation, compile indexes, plumbing | 35–70 | средний |
| [`mcp/router.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/router.rs#L96-L118) | перенос compiled map в `McpBackendGroup` | 5–15 | низкий |
| [`mcp/handler.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/handler.rs#L69-L124) | list rewrite/dedup, explicit reverse lookup, call-name restoration | 70–125 | высокий |
| Возможно [`mcp/upstream/mod.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/upstream/mod.rs#L304-L368) | не нужен при rewrite до `generic_stream`; максимум assertion/helper | 0–15 | средний |
| **Итого production** | | **145–290** | |

Тесты:

| Файл | Назначение | Оценка LOC |
|---|---|---:|
| [`mcp/mcp_tests.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/mcp_tests.rs#L614-L840) | integration tests list/call/failOpen/pagination/conditions/guardrails | 170–300 |
| [`types/local_tests.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/local_tests.rs#L1500-L1615) | YAML parsing и invalid configs | 50–90 |
| `state_manager.rs` tests | valid→valid и rejected reload, если существующий generic reload test неудобно расширить | 0–45 |
| schema/docs/example | generated schema + MCP README/example | 25–70 |
| **Итого tests/docs** | | **245–505** |

Реалистичный общий diff: **390–795 LOC**. Нижняя граница — permissive collision behavior и переиспользование test helpers; верхняя — строгая cross-page диагностика и отдельные reload/guardrails tests.

## Обязательные тесты

### Config parsing и validation

1. Пустой `toolNameOverrides` эквивалентен отсутствующему.
2. Валидная тройка `target/source/name` парсится и round-trip serializes.
3. Несуществующий target — ошибка normalization.
4. Пустые `source`/`name` — ошибка.
5. Duplicate `(target, source)` — ошибка.
6. Duplicate public `name` — ошибка.
7. Identity mapping — ошибка, если принято рекомендуемое ограничение.
8. Overrides с `prefixMode: conditional` и `always` — ошибка.
9. Unknown fields отклоняются согласно действующим serde/schema правилам.

### Listing

10. Уникальные tools без override сохраняют bare names.
11. Два targets с одинаковым source и двумя overrides публикуют оба заданных имени.
12. Смешанный каталог: renamed conflicts + untouched unique tools.
13. Override применяется и при отсутствии фактического конфликта.
14. Public override, столкнувшийся с bare tool другого target, даёт явную collision error, а не silent drop.
15. Неполный override raw collision ведёт себя по выбранной strict/permissive policy.
16. Configured source, отсутствующий у target, не создаёт phantom entry.
17. Pagination и gateway cursor сохраняются; override на второй странице применяется.
18. `_meta` в tool descriptor, включая MCP Apps `ui.resourceUri`, не повреждается.
19. RBAC/CEL-hidden tool не возвращается после rename.
20. Target с false `condition` не участвует в namespace и listing.

### Call routing и обратимость

21. Каждый explicit public name вызывает точный `(target, source)`; тестовые targets должны возвращать разные markers.
22. Upstream получает original source, не public name.
23. Unique bare call продолжает использовать существующий resolver.
24. Неизвестный public name возвращает нынешнюю invalid/unknown tool error.
25. Прямой вызов original конфликтного source не обходит public map.
26. Arguments и request `_meta` сохраняются без изменений.
27. Stateful и stateless multiplex работают одинаково.
28. Request-phase `mcpGuardrails` видит original source и один native service name.
29. Authorization применяется к native `(target, source)`, а не допускает bypass по alias.

### `failOpen`

30. Один недоступный renamed target не ломает `tools/list` здорового target.
31. Вызов public name недоступного target возвращает ошибку; gateway остаётся работоспособным.
32. После восстановления target его renamed tool снова появляется без перезагрузки rename cache.
33. Ошибка discovery healthy/failed targets сохраняет текущую разницу `failOpen`/`failClosed`.

### Hot reload и compatibility

34. Valid config A → valid config B меняет public names без рестарта.
35. Invalid reload (duplicate public name/unknown target) отклоняется; продолжает действовать last-good map A; reload status сообщает ошибку.
36. Исправленный следующий reload активирует map B и очищает reload error.
37. Без overrides regression tests проходят для `never`, `conditional`, `always`.
38. Существующий тест ambiguous names сохраняется для no-overrides либо осознанно меняется только при принятии strict error policy. Текущая база поведения зафиксирована в [`mcp_tests.rs#L716-L739`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/mcp_tests.rs#L716-L739).
39. Existing later-page resolution и stateless resolution остаются зелёными: [`mcp_tests.rs#L795-L838`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/mcp_tests.rs#L795-L838).

## Upstream PR против внешнего adapter/plugin

### Вариант A: upstream PR

**Плюсы**

- Использует уже имеющийся provenance target в list merge и существующий call resolver.
- Нет нового процесса, network hop, deployment unit и failure domain.
- `failOpen`, conditional targets, sessions, pagination, `_meta`, RBAC и hot reload остаются в одном lifecycle.
- Config schema и diagnostics становятся частью продукта.
- Scope можно удержать только в standalone/local config, как ранее делалось для небольших MCP override fields.

**Минусы**

- `handler.rs` — активный hot spot. В сентябре–октябре 2026 туда вошли pagination, modern `_meta`, target conditions и guardrails; rebase-риск высокий.
- Open issue [#963](https://github.com/agentgateway/agentgateway/issues/963) предлагает более широкую `toolCustomizations` модель. Maintainers могут попросить совместить naming/schema с ней, а не вводить временное поле.
- Требуется договориться о strict/permissive partial override и collision semantics до кода.
- Проект требует issue перед design document и свой шаблон: [`design/README.md`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/design/README.md). Contribution flow требует tests, `make lint`, `make test`, Conventional Commits: [`CONTRIBUTION.md`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/CONTRIBUTION.md).
- В PR checklist проекта явно встречается требование human-authored prose по Code of Conduct; владелец должен самостоятельно проверить и сформулировать issue/PR description/docs/comments.

**Рекомендация**

Сначала комментарий/мини-design в [#963](https://github.com/agentgateway/agentgateway/issues/963): предложить минимальный rename-only slice, сохранить поля `source` и `name`, добавить обязательный `target`, зафиксировать native-name policy plane. После согласования — один PR без xDS/Kubernetes surface.

### Вариант B1: `mcpGuardrails` CEL/extMcp plugin

Agentgateway предоставляет MCP-aware external policy protocol с request/response mutation: [`crates/protos/proto/ext_mcp.proto#L9-L64`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/protos/proto/ext_mcp.proto#L9-L64). Однако protocol contract говорит:

- single-target `tools/call` передаёт ровно один уже выбранный native backend;
- `tools/list` передаёт все fanout backends;
- request mutation заменяет params, response mutation — result.

Guardrails module прямо указывает, что single-target hooks видят unmuxed name, а fanout list — merged client-facing view: [`mcp/guardrails/mod.rs#L1-L10`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/guardrails/mod.rs#L1-L10). Следовательно, target resolution уже состоялся до plugin request hook. Plugin может:

- переименовать entries в объединённом `tools/list` response;
- переписать уже маршрутизированный `tools/call` params;

но не может надёжно выбрать target по новому public name. Кроме того, fanout CEL API пока не гарантирует удобный per-item target provenance; в guardrails commit [`276f10dc…`](https://github.com/agentgateway/agentgateway/commit/276f10dc68430762674d6d64f3baecead963ac78) `mcp.targets` отмечен как TODO.

**Вердикт: extMcp/CEL plugin API недостаточен для полного обратимого rename routing.** Использовать его как workaround не рекомендуется.

### Вариант B2: отдельный тонкий MCP adapter перед agentgateway

Технически возможен отдельный сервер, который:

1. проксирует `tools/list`, запоминает provenance и переписывает names;
2. принимает `tools/call`, восстанавливает target/source;
3. вызывает разные agentgateway endpoints либо сам multiplexes upstreams.

Но «тонким» он остаётся только если за agentgateway уже опубликованы отдельные endpoints на каждый target и adapter не делает multiplexing. Тогда нужны:

- отдельная конфигурация endpoints и rename map;
- health/fail-open политика;
- hot reload;
- pagination и `_meta` forwarding;
- sessions/stateful transport;
- auth propagation;
- защита от stale mapping;
- ещё один процесс и hop.

Если adapter стоит за одним multiplexed endpoint, он не получает надёжный hidden target provenance для отброшенных конфликтов и не может вызвать ambiguous original name. Если он стоит перед отдельными endpoints, он повторяет значительную часть agentgateway multiplexing.

**Вердикт:** внешний adapter допустим как временный локальный workaround при невозможности upstream merge, но ориентировочно дороже — **350–700 production LOC плюс deployment/config/tests**, выше операционный риск и хуже merge не требует только потому, что создаёт собственную поддержку навсегда.

## Merge и rebase риски

| Риск | Оценка | Снижение |
|---|---|---|
| Частые изменения `handler.rs` | высокий | маленькие helper-функции, не перестраивать fanout pipeline; rebase непосредственно перед PR |
| Пересечение с #963 tool customizations | высокий по schema, средний по core | сначала согласовать naming; использовать `source`/`name`; runtime map сделать переиспользуемым |
| Pagination и динамический каталог | средний/высокий | explicit reverse map не зависит от discovery cache; отдельные page-2 tests |
| Target conditions/RBAC | средний | lookup только среди request-local разрешённых upstreams; native-name policy tests |
| Guardrails ordering | средний | rewrite до `generic_stream`; тестировать видимое processor имя |
| `failOpen` regression | средний | не менять fanout/error machinery; targeted outage/recovery tests |
| Hot reload | низкий | map только внутри backend snapshot; invalid reload test на last-good state |
| Generated schema churn | низкий/средний | генерировать штатной командой проекта после согласования type shape |
| xDS/Kubernetes ожидания | средний продуктовый | явно назвать PR standalone-only; не добавлять proto/CRD в первый slice |

Наиболее вероятный merge blocker — не сложность реализации, а schema ownership относительно #963. Core-механика компактна и хорошо локализована.

## Лицензия

Репозиторий лицензирован под **Apache License 2.0**: [`LICENSE#L1-L201`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/LICENSE#L1-L201).

Лицензия разрешает использование, модификацию и распространение исходников и производных работ. Для внешнего fork/adapter либо распространения модифицированного бинарника необходимо сохранить текст лицензии, copyright/attribution notices, отметить изменённые файлы и соблюдать `NOTICE`, если применимо. Для upstream PR код после принятия распространяется в рамках лицензии проекта; contribution/process requirements всё равно задаются `CONTRIBUTION.md` и Code of Conduct.

## Итоговое решение

1. **Да, upstreamable patch возможен и оправдан.**
2. Делать rename-only slice для `prefixMode: never`, без general tool customization.
3. Контракт — backend-level `toolNameOverrides[]` с `target`, `source`, `name`.
4. Runtime — immutable двунаправленные индексы, собранные при normalization/hot reload.
5. `tools/list` применяет forward rename перед collision handling; unique tools остаются bare.
6. `tools/call` выполняет explicit reverse lookup первым, затем старый dynamic bare lookup; upstream всегда получает original source.
7. Существующие `failOpen` и hot reload не переписывать, только доказать тестами.
8. extMcp/CEL не подходит как полноценный adapter из-за ordering target resolution.
9. Отдельный MCP adapter — fallback, но не предпочтительный путь.
10. До реализации согласовать в #963 две вещи: имя/форма schema и strict либо permissive partial overrides.

## Основные источники

- Current commit: [`7f23e988…`](https://github.com/agentgateway/agentgateway/commit/7f23e9881716700fb7c75edeba3fb5645604d086)
- Latest release: [`v1.6.0`](https://github.com/agentgateway/agentgateway/releases/tag/v1.6.0)
- MCP types: [`types/agent.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/agent.rs#L1895-L2050)
- Local config normalization: [`types/local.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/types/local.rs#L1746-L1945)
- MCP routing/merge: [`mcp/handler.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/handler.rs)
- Upstream request path: [`mcp/upstream/mod.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/upstream/mod.rs#L304-L368)
- Runtime plumbing: [`mcp/router.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/router.rs#L96-L118)
- Hot reload: [`state_manager.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/state_manager.rs#L138-L324)
- Guardrails contract: [`mcp/guardrails/mod.rs`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/guardrails/mod.rs#L1-L10)
- extMcp protocol: [`ext_mcp.proto`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/protos/proto/ext_mcp.proto#L9-L64)
- Existing tests: [`mcp_tests.rs#L614-L840`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/crates/agentgateway/src/mcp/mcp_tests.rs#L614-L840)
- Current `prefixMode: never` example: [`examples/mcp-apps/config-multiplex.yaml`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/examples/mcp-apps/config-multiplex.yaml)
- Tool customization request: [issue #963](https://github.com/agentgateway/agentgateway/issues/963)
- Security/semantic warning about list/call symmetry: [#963 comment](https://github.com/agentgateway/agentgateway/issues/963#issuecomment-5510461089)
- Virtual consolidation request: [issue #3382](https://github.com/agentgateway/agentgateway/issues/3382)
- Apache-2.0: [`LICENSE`](https://github.com/agentgateway/agentgateway/blob/7f23e9881716700fb7c75edeba3fb5645604d086/LICENSE#L1-L201)
