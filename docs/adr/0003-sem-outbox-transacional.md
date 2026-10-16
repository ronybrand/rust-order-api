# ADR 0003: Notificação publicada após o commit, sem outbox transacional

## Contexto

Confirmar ou cancelar um pedido deve avisar o cliente por e-mail. O e-mail é efeito colateral lento e sujeito a falha (SMTP), por isso é assíncrono: a API publica um evento no RabbitMQ e o worker envia o e-mail.

## Decisão

A API publica o evento **depois** do commit, em *best effort*: se o broker estiver indisponível, o erro é registrado (sem o e-mail do cliente) e a transição do pedido continua válida. Não há outbox transacional.

A entrega do worker é *at-least-once*: até 3 retentativas com espera fixa de 2 s (cabeçalho `x-retry-count`), depois a fila `order.status.changed.dlq`; falhas permanentes (destinatário inválido, mensagem malformada) vão direto à DLQ. Reentregas são descartadas pelo `message_id` (conjunto em memória, com TTL e tamanho limitados).

## Alternativas

- **Outbox transacional** (tabela `outbox` na mesma transação e um relay): elimina a perda de evento entre o commit e a publicação. Descartado por ora porque a notificação é informativa, uma perda ocasional é tolerável e o custo (tabela, relay, limpeza) não se justifica ainda.
- **Chamada SMTP síncrona no request**: simples, mas acopla a latência e a disponibilidade da API ao servidor de e-mail.

## Consequências

Entre o commit e a publicação, uma queda do processo perde o evento. O dedupe em memória não sobrevive a restart do worker; um e-mail duplicado raro é aceito. Se a notificação virar requisito de negócio, adotar o outbox.
