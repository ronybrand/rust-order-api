# rust-order-api

API de pedidos em Rust, implementando a especificação do [`fastapi-order-api`](https://github.com/ronybrand/fastapi-order-api) (`DOMAIN.md` e catálogo de erros daquele projeto) com as convenções da linguagem: `axum`, `sqlx`, tipos que consomem o estado antigo ao mudar (`self` por valor) e erros como valores.

O mesmo domínio já existe em FastAPI, Nest, .NET, Spring, Go, PHP e Kotlin; este repositório existe para comparar a mesma especificação em mais uma stack, com o mesmo contrato HTTP e os mesmos gates de CI.

- **API** (`api`): clientes e pedidos, PostgreSQL, JWT RS256.
- **Worker** (`worker`): consome `order.status.changed` no RabbitMQ e envia e-mail por SMTP.

## Rodando

```sh
mkdir keys   # coloque aqui jwt_public.pem (chave pública RSA do emissor dos tokens)
docker compose up --build
```

API em `http://127.0.0.1:8080`, e-mails capturados no Mailpit em `http://127.0.0.1:8025`. Todas as portas escutam apenas em `127.0.0.1`.

Sem Docker: `DATABASE_URL=... JWT_PUBLIC_KEY_PATH=... JWT_AUDIENCE=... JWT_ISSUER=... cargo run --bin api` e `cargo run --bin worker`. As migrations são aplicadas na partida (o `sqlx` usa lock consultivo, seguro com várias réplicas).

## Configuração

| Variável | Padrão | Observação |
|---|---|---|
| `APP_ENV` | `development` | `production` liga as validações estritas abaixo |
| `HTTP_ADDR` | `:8080` | |
| `DATABASE_URL` | | obrigatória; `postgres://usuário:senha@host:5432/banco?sslmode=disable` |
| `JWT_PUBLIC_KEY_PATH` | | obrigatória na API; uma chave **privada** é recusada |
| `JWT_AUDIENCE`, `JWT_ISSUER` | | obrigatórias na API; ver ADR 0005 |
| `CORS_ORIGINS` | vazio | lista separada por vírgula; em produção não pode ser vazia nem `*` |
| `MAX_BODY_BYTES` | `1048576` | conta também corpos *chunked* |
| `RATE_LIMIT_PER_USER` / `_WRITES` / `_PER_IP` | `120` / `30` / `600` | por minuto; `/health` é isento |
| `BROKER_URL` | | sem ela a API não publica eventos; em produção precisa ser `amqps://` |
| `SMTP_HOST`, `SMTP_PORT`, `SMTP_USER`, `SMTP_PASSWORD`, `SMTP_FROM` | porta `587` | worker; em produção STARTTLS é obrigatório |
| `SHUTDOWN_TIMEOUT_SECONDS` | `15` | |
| `RUST_LOG` | `info` | nível de log |

Em produção a API se recusa a iniciar com audience/issuer ausentes, CORS vazio ou com `*`, ou broker sem TLS, e lista todos os problemas de uma vez.

## Endpoints

| Método e rota | Quem pode |
|---|---|
| `GET /health` | público (verifica o banco) |
| `POST /customers`, `PUT /customers/{id}`, `DELETE /customers/{id}` | papel `admin` |
| `GET /customers/{id}`, `GET`/`POST /customers/search` | qualquer usuário autenticado |
| `POST /orders`, `GET`/`DELETE /orders/{id}`, `GET`/`POST /orders/search` | qualquer usuário autenticado |
| `POST /orders/{id}/items`, `PATCH`/`DELETE /orders/{id}/items/{item_id}` | qualquer usuário autenticado |
| `POST /orders/{id}/confirm`, `POST /orders/{id}/cancel` | qualquer usuário autenticado |

O contrato completo está em [`api/openapi.yaml`](api/openapi.yaml), e um teste falha se ele e o roteador deixarem de descrever a mesma API (rotas, rotas públicas e códigos de erro). A [documentação interativa](https://ronybrand.github.io/rust-order-api/) é publicada no GitHub Pages a partir desse contrato.

Busca: `page` (base 0), `size` (1 a 100), `sort` (`-campo` para decrescente). A forma `POST` aceita `filters`: `{"campo": [{"op": "gte", "value": 2}]}` com `eq, neq, gt, gte, lt, lte, in, between, lk`. Valores são convertidos para o tipo da coluna (tipo errado é `VALIDATION-05`), `lk` escapa `%` e `_`, campo de filtro desconhecido é ignorado e campo de ordenação desconhecido é `VALIDATION-06`.

Erros: `{"message", "code", "params", "request_id"}`; o catálogo está em [`DOMAIN.md`](DOMAIN.md). A resposta de cliente expõe apenas `id` e `name`: documento, passaporte e e-mail são PII e nunca saem na API nem nos logs.

## Garantias de concorrência

- Pedido e cliente têm coluna `version`; escrita com versão antiga falha com `CONFLICT-00`.
- Criar pedido toma `FOR SHARE` no cliente e excluir cliente toma `FOR UPDATE`, na mesma transação da verificação de pedidos ativos: nunca nasce pedido sob um cliente excluído. Um teste repete a corrida 20 vezes contra um PostgreSQL real.
- Unicidade de documento e passaporte é garantida por índices únicos parciais (`WHERE deleted_at IS NULL`); a checagem no serviço só dá a mensagem amigável.

## Arquitetura

```mermaid
flowchart LR
    Cmd["bin/api.rs e bin/worker.rs\n(só compõem)"]

    subgraph Adapters["Adaptadores"]
        direction TB
        Http["http\n(axum)"]
        Pg["persistence\n(sqlx)"]
        Msg["messaging\n(lapin, lettre)"]
        Auth["auth\n(JWT RS256)"]
    end

    App["app\ncasos de uso e traits\n(Store, Session, EventPublisher)"]
    Domain["domain\nCustomer, Order, Item"]

    Cmd -. compõe .-> Adapters
    Http -- depende de --> App
    Pg -- depende de --> App
    Msg -- depende de --> App
    Http -- depende de --> Auth
    App -- depende de --> Domain
```

A dependência aponta sempre para dentro: o domínio não conhece nada, a aplicação declara o que consome e os adaptadores implementam. `error` e `config` ficam de fora do desenho: o primeiro é usado por todos, o segundo só pela composição.

**Uma requisição e o e-mail que ela dispara**

```mermaid
flowchart LR
    Client["Cliente"] -->|HTTPS| MW["Middleware\nrequest id, cabeçalhos de segurança,\nCORS, limite de corpo, rate limit"]
    MW --> Authn["JWT RS256\n+ papel"]
    Authn --> Handler["Handlers e middleware (http)"]
    Handler --> Service["Serviços (app)"]
    Service --> Store["PostgresStore"]
    Store --> DB[("PostgreSQL")]
    Service -->|"após o commit\n(best effort, sem outbox)"| Pub["EventPublisher"]
    Pub --> Queue[["RabbitMQ\norder.status.changed"]]
    Queue --> Worker["Worker (bin/worker.rs)"]
    Worker --> Mail(["SMTP: e-mail do cliente"])
    Worker -. "retry / DLQ" .-> Queue
```

A API e o worker são dois processos independentes, ligados só pela fila. Se o broker estiver fora do ar, a transição do pedido continua válida e a falha só é registrada ([ADR sobre a ausência de outbox](docs/adr/0003-sem-outbox-transacional.md)).

```
src
├── error.rs        AppError, o catálogo de códigos e o tipo de erro único
├── domain/         Customer, Order, Item, eventos: Rust puro, sem I/O
├── app/            casos de uso e os traits (Store, Session, EventPublisher) que eles consomem
├── persistence/    sqlx: PostgresStore, o plano de busca tipado e o SQL gerado dele
├── auth.rs         verificação de JWT RS256
├── http/           axum: tabela de rotas, middleware, envelope de erro, tipos do contrato
├── messaging/      eventos, regras de retry/DLQ/dedupe, RabbitMQ (lapin), e-mail (lettre)
├── config.rs       variáveis de ambiente e as regras de produção
└── bin/            api.rs e worker.rs: só compõem
migrations/         SQL do sqlx, embutido no binário
```

A dependência aponta sempre para dentro: `domain` não conhece nada, `app` declara o que consome, `persistence`, `http` e `messaging` implementam e adaptam. Decisões e alternativas descartadas em [`docs/adr/`](docs/adr/README.md).

**O que é de Rust, e não só uma tradução:**

- Uma transição de pedido **consome** o pedido (`self` por valor) e devolve o novo estado: o estado antigo não pode ser usado por engano, e uma regra violada nunca deixa um pedido pela metade.
- Falhas são valores (`Result<_, Error>`), nunca exceções; `Error::Unexpected` vira `INTERNAL-00` sem a mensagem, em um único ponto (o middleware que também conhece o `request_id`).
- A biblioteca proíbe `unsafe` (`#![forbid]` via lints do `Cargo.toml`) e o `clippy` roda em modo pedante com `-D warnings`.

## Imagem

Um binário estático (musl) sobre `scratch`: sem shell, sem gerenciador de pacotes, sem libc. As raízes de confiança do TLS vão compiladas (`webpki-roots`), então a imagem também não precisa de um bundle de CAs. O tamanho (descomprimido e comprimido) sai no resumo do job `image` do CI.

Como não há sistema operacional na imagem, não há nada a varrer nela; a verificação de vulnerabilidades roda sobre o que de fato entra no binário: `Cargo.lock` (cargo-deny com o RustSec, e o Trivy sobre o SBOM CycloneDX).

## Desenvolvimento

Requer o toolchain de `rust-toolchain.toml`. Nada além disso para os testes unitários.

```sh
make test          # fmt, clippy -D warnings e os testes unitários (rápido, sem I/O)
make integration   # PostgreSQL e RabbitMQ reais (variáveis abaixo)
make verify        # tudo mais o piso de cobertura (cargo-llvm-cov), igual ao CI
```

Os testes de integração são dirigidos por variáveis de ambiente:

| Variável | Para quê |
|---|---|
| `TEST_DATABASE_URL` | conexão administrativa de um PostgreSQL (`postgres://orders:orders@127.0.0.1:5432/postgres`); cada teste cria o próprio banco e o remove |
| `TEST_AMQP_URL` | um RabbitMQ (`amqp://guest:guest@127.0.0.1:5672/%2f`) |
| `REQUIRE_INTEGRATION=1` | faz a falta de um serviço **falhar** o teste em vez de pulá-lo; o CI sempre define |

Sem as variáveis os testes que dependem delas dizem que foram pulados e passam, para que `cargo test` rode numa máquina vazia. Os testes de SMTP não precisam de nada: falam com um servidor SMTP mínimo dentro do próprio processo.

`bash scripts/smoke.sh` sobe o `docker-compose.yml` inteiro (PostgreSQL, RabbitMQ, Mailpit, API e worker), assina tokens RS256 como o emissor faria, cria um cliente e um pedido, confirma o pedido e espera o e-mail chegar no Mailpit. O CI o roda a cada push.

O CI roda: `rustfmt`, `clippy`, os testes com PostgreSQL e RabbitMQ reais e o piso de cobertura, `cargo-deny`, o build das duas imagens, o smoke, o Trivy sobre o repositório, o CodeQL, o SBOM e a revisão de dependências. Diferente dos projetos Gradle, o grafo de dependências do GitHub lê o `Cargo.lock` sozinho: o Dependabot e a revisão de dependências enxergam todos os crates, sem passo extra.

## O que foi verificado, e como

- Domínio, casos de uso, busca, HTTP (rotas, envelope, limites, CORS, rate limit, contrato OpenAPI), JWT, configuração, regras de retry e DLQ: testes unitários sem I/O, contra um repositório em memória.
- Cada instrução SQL, as constraints, os locks e a corrida cliente/pedido: testes de integração contra um PostgreSQL real.
- O e-mail: contra um servidor SMTP que fala o protocolo, dentro do teste.
- **RabbitMQ (publisher e consumer), a imagem e o `docker compose` só são exercitados no CI.** O ambiente de desenvolvimento deste projeto não tinha Docker em execução nem um broker; o código do `lapin` foi escrito contra a documentação e depende do CI para ser provado.
