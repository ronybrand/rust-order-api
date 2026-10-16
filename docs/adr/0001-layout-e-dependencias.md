# ADR 0001: Layout em módulos e a direção das dependências

## Contexto

Mesmo domínio das outras implementações, para que a comparação seja entre linguagens e não entre arquiteturas.

## Decisão

Um crate de biblioteca (`orderapi`) e dois binários que só compõem (`api`, `worker`). Módulos: `domain` (Rust puro), `app` (casos de uso e os traits que eles consomem), `persistence`, `http`, `messaging` e `config`. A dependência aponta para dentro: `domain` não importa nada do resto, `app` declara `Store`, `Session` e `EventPublisher` onde são consumidos, e os módulos de borda os implementam.

Os dois repositórios (clientes e pedidos) são **um trait só**, `Session`: uma transação é uma conexão, e os dois precisam rodar nela. Isso difere do Kotlin e do Go, que têm dois contratos e um agrupador.

## Alternativas

- **Workspace com um crate por camada**: impõe a direção das dependências em tempo de compilação, mas para este tamanho multiplica manifestos sem ganho real.
- **Traits `Customers` e `Orders` separados**: espelham os outros projetos, mas obrigam a passar a mesma conexão por dois objetos.

## Consequências

A direção das dependências é uma convenção revisada, não imposta pelo compilador. Os traits assíncronos usam `async_trait` porque precisam ser objetos (`dyn`), o que `async fn` em trait ainda não permite.
