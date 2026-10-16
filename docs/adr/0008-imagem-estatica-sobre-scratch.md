# ADR 0008: Imagem estática sobre `scratch`

## Decisão

A imagem de cada processo é um único binário estático (`musl`) copiado para `scratch`, rodando como o usuário numérico 65532. As raízes de confiança do TLS vão compiladas (`webpki-roots`), então não é preciso um bundle de CAs. O build usa `--locked`, com o `Cargo.lock` revisado e varrido.

## Alternativas

- **Alpine como base final**: dá um shell para depurar e pacotes que o Trivy sabe varrer, ao custo de mais superfície.
- **Distroless**: parecido com `scratch` mas com libc; desnecessário para um binário musl estático.

## Consequências

Não há shell na imagem (para depurar, usa-se um contêiner efêmero com ferramentas) e não há pacotes de sistema para o Trivy varrer a imagem: a varredura de vulnerabilidades roda sobre o `Cargo.lock` (`cargo-deny`, e o Trivy sobre o SBOM), que é o que de fato entra no binário.

Dois provedores de criptografia para TLS no mesmo processo fazem o `rustls` entrar em pânico no primeiro handshake; por isso o `deny.toml` proíbe o `aws-lc-rs` e as dependências usam só o `ring`.
