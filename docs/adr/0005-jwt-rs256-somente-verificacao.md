# ADR 0005: JWT RS256 só de verificação

## Decisão

O serviço verifica tokens, nunca os emite: guarda apenas a chave pública do emissor, então um vazamento dela não permite forjar um token. Só RS256 é aceito (`alg: none` e HS256 assinado com a chave pública nunca validam), com `exp`, `aud` e `iss` obrigatórios, tolerância de relógio de 30 s e `sub` de até 200 caracteres (cabe nas colunas de auditoria). `JWT_AUDIENCE` e `JWT_ISSUER` são obrigatórios; sem eles a API não sobe.

Toda falha de verificação é o mesmo 401: nem o chamador nem o cliente descobrem por que o token falhou.

A biblioteca (`jsonwebtoken`) aceitaria uma chave **privada** em `JWT_PUBLIC_KEY_PATH` e verificaria com a metade pública. Aqui isso é recusado na partida: uma chave privada no disco da API é um erro que merece parar o processo, porque este serviço nunca deve ter a capacidade de emitir um token.

## Consequências

A rotação de chaves exige reiniciar a API (a chave é lida uma vez); um JWKS com cache seria o próximo passo.
