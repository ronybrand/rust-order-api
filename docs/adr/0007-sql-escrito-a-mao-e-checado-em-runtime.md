# ADR 0007: SQL escrito à mão, checado em tempo de execução

## Contexto

O `sqlx` oferece macros (`query!`) que verificam cada instrução contra o banco **na compilação**. Isso exige um banco acessível no build, ou um cache de consultas versionado (`.sqlx/`), regerado a cada mudança.

## Decisão

As instruções são escritas à mão e usam `query`/`query_as` (verificação em tempo de execução). A busca monta o SQL dinamicamente de qualquer forma (filtros e ordenação vêm da requisição), e essa parte nunca poderia ser verificada na compilação. Os testes de integração, que executam cada instrução contra um PostgreSQL real, é que pegam um erro de digitação.

Os valores são sempre parâmetros ligados. Os nomes de coluna dos filtros e da ordenação vêm de uma lista fixa (o plano de busca); as poucas strings montadas com `format!` só interpolam constantes do próprio arquivo, e passam por `AssertSqlSafe` para que o `sqlx 0.9` deixe explícito o ponto a auditar.

## Consequências

Um erro de SQL só aparece ao rodar os testes de integração, não ao compilar. Em troca, o build não depende de um banco e não há um diretório de cache a manter.
