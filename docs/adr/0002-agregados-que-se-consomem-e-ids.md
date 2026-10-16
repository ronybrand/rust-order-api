# ADR 0002: Transições que consomem o agregado, e ids gerados pela aplicação

## Decisão

`Order` e `Customer` são valores. Cada operação recebe o agregado por valor (`self`) e devolve o novo estado, ou um erro e nada: o estado antigo foi movido e não pode ser usado por engano, e uma regra violada nunca deixa um pedido pela metade.

Os ids são gerados pela aplicação (`Uuid::new_v4()`), não pelo banco. O default `gen_random_uuid()` continua no schema como rede de segurança para inserções feitas por fora.

## Alternativas

- **`&mut self`**: mais comum em Rust, mas uma regra que falha no meio deixa o valor alterado; seria preciso desfazer à mão.
- **Id pelo banco (`RETURNING id`)**: exige a ida ao banco antes de existir um id para o evento e para os itens do pedido.

## Consequências

Cada transição move o pedido; quem precisa do estado anterior (o status "de antes", para o evento) o recebe no retorno (`StatusChange`). Clonar custa pouco para pedidos de até 200 itens.
