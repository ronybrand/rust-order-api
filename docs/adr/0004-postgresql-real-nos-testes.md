# ADR 0004: Testes de integração contra serviços reais, dirigidos por variáveis

## Contexto

Mockar o banco esconde exatamente o que mais quebra: constraints, locks, tipos e o SQL gerado pela busca.

## Decisão

Os testes de integração falam com um PostgreSQL e um RabbitMQ reais, apontados por `TEST_DATABASE_URL` e `TEST_AMQP_URL`. Cada teste cria o próprio banco (`CREATE DATABASE`), roda as migrations e o remove ao fim (`DROP DATABASE ... WITH (FORCE)`, que também encerra conexões que o teste esqueceu).

Sem a variável o teste diz que foi pulado e passa, para `cargo test` rodar numa máquina vazia. Com `REQUIRE_INTEGRATION=1` (sempre no CI) a falta de um serviço **falha** o teste: um teste pulado nunca pode parecer verde onde importa.

O servidor SMTP dos testes é um servidor mínimo dentro do processo, que fala o protocolo; não há serviço externo.

## Alternativas

- **Testcontainers**: sobe os serviços sozinho, mas exige um Docker local; aqui o CI já tem os serviços como *service containers* e localmente basta um PostgreSQL.
- **Um banco embarcado (como nos projetos JVM)**: não existe um equivalente maduro para Rust.

## Consequências

Rodar tudo localmente exige subir PostgreSQL e RabbitMQ (`docker compose up -d postgres rabbitmq`). Os testes do RabbitMQ compartilham as filas globais do broker e por isso rodam um de cada vez.
