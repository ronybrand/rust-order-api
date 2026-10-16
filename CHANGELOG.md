# Changelog

Formato baseado em [Keep a Changelog](https://keepachangelog.com/pt-BR/1.0.0/). O projeto não versiona
releases (`main` é a linha de entrega); as entradas agrupam o histórico por tema, não por tag.

## [Unreleased]

### Added

- Domínio de clientes e pedidos portado da especificação do `fastapi-order-api`: transições que consomem o estado antigo, total derivado em `Decimal`, máquina de estados e evento de mudança de status.
- Casos de uso sobre traits, com busca validada (limites de página, de filtros e de valores).
- Persistência em PostgreSQL com `sqlx`: índices únicos parciais, lock otimista por `version`, `FOR UPDATE`/`FOR SHARE` contra a corrida entre excluir cliente e criar pedido.
- API HTTP em `axum`: tabela única de rotas com o acesso de cada uma, envelope de erro renderizado em um só ponto, JWT RS256 só de verificação, limites de taxa (por IP, por usuário e por escrita), limite de corpo que conta *chunked*, CORS por origem exata e contrato OpenAPI com teste de deriva.
- Worker de notificação: RabbitMQ com confirmações do publisher, retry, DLQ e deduplicação, e-mail por SMTP com STARTTLS obrigatório em produção.
- Teste de fumaça ponta a ponta da pilha do compose (`scripts/smoke.sh`), rodado no CI.
- Imagem estática sobre `scratch` (um binário, sem sistema operacional).
- Testes unitários sem I/O e de integração contra PostgreSQL, RabbitMQ e um servidor SMTP de verdade.

### Security

- Actions fixadas por SHA, Dependabot semanal com auto-merge de patch e minor, revisão de dependências em cada pull request, CodeQL, `cargo-deny` e varredura do SBOM.
- A produção recusa subir com audience ou issuer ausentes, CORS vazio ou `*`, ou broker sem `amqps://`; uma chave privada no lugar da pública também recusa a partida.
- A resposta de cliente expõe só `id` e `name`; documento, passaporte e e-mail ficam fora da API e dos logs.
