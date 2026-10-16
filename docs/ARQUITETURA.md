# Arquitetura técnica: rust-order-api

Explicação técnica de como a API de pedidos em Rust é montada e por quê. O contrato (rotas, erros, regras de domínio) vem do `fastapi-order-api`; aqui está o que é próprio desta implementação. As decisões formais ficam em [`adr/`](adr/README.md).

## 1. Visão geral

Dois binários compartilham uma biblioteca:

- `api`: servidor HTTP (axum) com PostgreSQL e publicação de eventos no RabbitMQ.
- `worker`: consome `order.status.changed` e envia e-mail por SMTP.

Stack: Rust, axum 0.8, tokio, sqlx 0.9 (PostgreSQL), `rust_decimal`, `serde`/`serde_json`, `jsonwebtoken`, `lapin` 4 (AMQP), `lettre` (SMTP). A imagem final é um binário estático musl sobre `scratch`.

## 2. Camadas e direção das dependências

```
domain      Rust puro: Customer, Order, Item, eventos. Sem I/O, sem async.
app         casos de uso + os traits que eles consomem (Store, Session, EventPublisher, Notifier, Mailer, Mover)
persistence implementa Store/Session com sqlx; plano de busca tipado; tradução de SQLSTATE
http        axum: tabela de rotas, middlewares, envelope de erro, tipos do contrato
messaging   regras de retry/DLQ/dedupe, lapin, lettre
bin/        api.rs e worker.rs só compõem as peças
```

A dependência aponta para dentro. Os traits são declarados em `app`, onde são consumidos, e implementados nas bordas. Isso permite rodar todos os casos de uso contra um `fake.rs` em memória, sem banco. A direção não é imposta pelo compilador (um `use` errado compila), é convenção revisada.

## 3. Domínio

- Uma transição de pedido (`confirm`, `cancel`, adicionar item) **consome** o pedido (`self` por valor) e devolve o novo estado. O estado antigo deixa de existir para o chamador, e uma regra violada devolve `Err` sem deixar o pedido pela metade.
- Dinheiro é `rust_decimal::Decimal`. Com a feature `arbitrary_precision` do `serde_json`, o preço entra e sai como número JSON exato (sem passar por `f64`). Um valor fora da faixa do `Decimal` vira corpo inválido.
- Falhas são valores: `Result<_, Error>`. `Error::Unexpected` vira `INTERNAL-00` sem repetir a mensagem ao cliente.

## 4. Fluxo de uma requisição

1. `request_context` (middleware): valida `X-Request-Id` (só UUID), cria um se faltar, aplica limite de corpo contando bytes em chunks, rate limit por IP, e **monta o envelope de erro num só ponto**, com `request_id`.
2. Autenticação: `Bearer` RS256 verificado só com chave pública. `exp`, `aud`, `iss`, `sub` exigidos, tolerância de 30 s. A API recusa subir com uma chave privada no lugar da pública.
3. Autorização: tabela única de rotas indica o acesso de cada uma (`admin` ou qualquer autenticado). Não existe rota pública por omissão, só `/health`.
4. Rate limit por usuário e por escrita (`KeyedLimiter`, token bucket por chave, em memória do processo).
5. Extração com `serde` e `deny_unknown_fields`: campo desconhecido, tipo errado e UUID fora da forma canônica são `VALIDATION-01`.
6. Caso de uso em `app`, dentro de uma `Session` transacional.
7. Resposta, com sete cabeçalhos de segurança também nos erros (404, 405, 413, 429 usam o mesmo envelope).

## 5. Persistência

- `PostgresStore` abre uma `Session` que é uma transação. Os repositórios de cliente e pedido são um trait só porque uma transação é uma conexão.
- Migrations do sqlx em SQL à mão (`migrations/0001_init.sql`), embutidas no binário e aplicadas na partida com lock consultivo, seguro com várias réplicas. Sem `down` (ADR 0007).
- O SQL é checado em tempo de execução. Como o sqlx 0.9 exige, o texto vai embrulhado em `AssertSqlSafe`, e a regra que o torna seguro é: o texto só contém nomes vindos de listas internas, valores sempre como parâmetros.
- **Busca**: o pedido (`GET` ou `POST /search`) vira um plano tipado (`SearchEntity`, `Predicate`, `Typed`), validado sem SQL. Só depois o `QueryBuilder` gera o SQL com parâmetros ligados. `lk` escapa `%` e `_`. Valor com tipo errado é `VALIDATION-05`, ordenação em campo desconhecido é `VALIDATION-06`, `size=0` significa o padrão.
- **Concorrência**: coluna `version` em `Order` e `Customer` (`CONFLICT-00` para versão velha). Criar pedido toma `FOR SHARE` no cliente, excluir cliente toma `FOR UPDATE`, na mesma transação da checagem de pedidos ativos. Unicidade por índices únicos parciais (`WHERE deleted_at IS NULL`); a checagem no serviço só dá a mensagem amigável.
- Violações viram códigos de negócio por SQLSTATE e nome da constraint (`translate.rs`).

## 6. Mensageria

- A API publica `order.status.changed` com confirmação do broker (publisher confirms). Sem `BROKER_URL` ela não publica.
- O worker só reconhece (ack) a mensagem original depois que a cópia para a fila de retry ou DLQ foi confirmada. Tentativas contadas em `x-retry-count`, DLQ ao esgotar, deduplicação por `message_id` em memória.
- `lapin` usa só `rustls--ring`. O `cargo-deny` proíbe `aws-lc-rs` para ter um único provedor de TLS.
- A vhost `/` é normalizada de `%2f`.
- E-mail por `lettre`; em produção STARTTLS é obrigatório. Não há outbox (ADR 0003), então um crash entre o commit e a publicação perde o evento.

## 7. Segurança

- Produção falha na partida listando todos os problemas de uma vez: audience/issuer ausentes, CORS vazio ou `*`, broker sem `amqps://`.
- `unsafe` é proibido por lint. `Debug` de tipos sensíveis é escrito à mão (`opaque_debug!`) e só mostra o id. A resposta de cliente expõe apenas `id` e `name`.
- Limites conhecidos: contadores de rate limit por processo, sem proxies confiáveis, sem timeouts de cabeçalho configurados no servidor.

## 8. Testes

- 213 unitários sem I/O (domínio, casos de uso contra `fake.rs`, busca, HTTP, JWT, config, regras de retry) e 40 de integração.
- Integração com PostgreSQL e RabbitMQ reais, cada teste criando e removendo o próprio banco (`DROP DATABASE ... FORCE`); SMTP por um servidor mínimo no processo. Com `REQUIRE_INTEGRATION=1` a falta de serviço **falha** em vez de pular (o CI sempre define).
- Teste de deriva entre `api/openapi.yaml` e o roteador; teste que confere a lista de campos pesquisáveis contra o schema real.
- Cobertura por `cargo-llvm-cov`, piso 90% (94,2% no CI). `clippy` pedante com `-D warnings`.
- Não verificado por teste: handshakes TLS do `amqps` e do STARTTLS.

## 9. CI, build e entrega

Actions por SHA, rustfmt, clippy, testes com serviços reais, `cargo-deny` (RustSec, licenças, origens, bans), SBOM CycloneDX varrido pelo Trivy, CodeQL, revisão de dependências, build das imagens e smoke do `docker compose` (assina JWT, cria cliente e pedido, confirma e espera o e-mail no Mailpit). O GitHub lê o `Cargo.lock` sozinho para o grafo de dependências. Há um aviso sem correção (RUSTSEC-2023-0071) ignorado com o motivo registrado.

Imagem: ~9 MB (4 MB comprimida), sem shell nem libc; CAs compiladas via `webpki-roots`.

## 10. Trade-offs

| Escolha | Ganho | Custo |
|---|---|---|
| Estado consumido por `self` | estados inválidos não representáveis | mais verbosidade nos testes |
| sqlx com SQL à mão | locks e índices visíveis | SQL só checado ao rodar |
| Envelope montado num middleware | um ponto de verdade | o erro nasce em outro lugar de onde é renderizado |
| Sem outbox | simples | evento pode se perder |
| Binário estático em `scratch` | superfície mínima | difícil depurar dentro da imagem; compilar é lento |
