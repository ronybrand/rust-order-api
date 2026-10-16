# Domínio: gestão de pedidos

A especificação funcional (entidades, invariantes, eventos e catálogo de erros) é a do
[`fastapi-order-api`](https://github.com/ronybrand/fastapi-order-api/blob/main/DOMAIN.md); este projeto a implementa
em Rust e não a duplica, para que haja uma única fonte. Este arquivo mapeia a especificação para o código e registra
onde esta implementação acrescenta ou escolhe diferente.

## Onde cada regra vive

| Regra da especificação | Código |
|---|---|
| Validação de cliente (nome, documento canônico, passaporte, e-mail) | `src/domain/customer.rs`, `src/domain/email.rs` |
| Itens, total derivado, máquina de estados OPEN, CONFIRMED, CANCELED | `src/domain/order.rs` |
| Evento `OrderStatusChanged` (contrato da fila) | `src/domain/event.rs`, `src/messaging/event_wire.rs` |
| Criar pedido exige cliente existente; excluir cliente exige zero pedidos ativos | `src/app/orders.rs`, `src/app/customers.rs` |
| Unicidade de documento e passaporte entre ativos | índices parciais em `migrations/0001_init.sql` |
| Lock otimista (`version`) | `src/persistence/postgres.rs` |
| Busca, filtros e limites | `src/app/search.rs`, `src/persistence/plan.rs`, `src/persistence/sql.rs` |
| Códigos de erro | `src/error.rs` |
| Retry, DLQ e deduplicação das notificações | `src/messaging/processor.rs`, `src/messaging/delivery.rs` |
| Contrato HTTP e acesso por rota | `src/http/mod.rs` (tabela de rotas), `api/openapi.yaml` |
| Verificação de JWT e papéis | `src/auth.rs` |

## Diferenças em relação à especificação

- **Resposta de cliente**: só `id` e `name`. Documento, passaporte e e-mail são PII e não voltam pela API; `Debug` de `Customer` mostra só o id.
- **Códigos acrescentados**: `RESOURCE-NOT-FOUND-00` (rota inexistente) e `VALIDATION-13` (método não permitido, status 405 com `Allow`), para que 404 e 405 saiam no mesmo envelope `{message, code, params, request_id}`.
- **Identificadores**: gerados pela aplicação (`Uuid::new_v4()`), não pelo banco; o default `gen_random_uuid()` continua no schema como rede de segurança. Ver o ADR 0002.
- **Agregados que se consomem**: cada operação recebe o pedido por valor e devolve o novo estado; uma regra violada nunca deixa um pedido pela metade.
- **Campos pesquisáveis**: `customers` aceita `id, name, tax_id, passport_number, email, version, created_at, updated_at, created_by, updated_by`; `orders` aceita `id, customer_id, status, version, created_at, updated_at, created_by, updated_by`. Colunas de exclusão lógica não são pesquisáveis nem ordenáveis. Um teste compara a lista com as colunas do schema real.
- **Total do pedido** tem sempre duas casas (`rust_decimal::Decimal`), nunca ponto flutuante, e é escrito como número JSON com os dígitos exatos (`40.28`). Um preço fora da faixa de um `Decimal` (mais de 28 dígitos) é recusado como corpo inválido (`VALIDATION-01` em `body`), e não como `unit_price` grande demais.
- **Notificação**: publicada depois do commit, sem outbox (ADR 0003). O valor do evento viaja como texto (`"total_amount": "10.30"`), para que nenhum consumidor o leia como ponto flutuante.
- **Chave de verificação**: uma chave privada em `JWT_PUBLIC_KEY_PATH` faz a API recusar a partida (ADR 0005).
- **Catálogo ampliado, como no `go-order-api`**: `VALIDATION-09` (limite de itens), `VALIDATION-12` (corpo grande demais, 413), `CONFLICT-04` (violação de integridade do banco), `AUTH-01` (401) e `AUTH-02` (403).
