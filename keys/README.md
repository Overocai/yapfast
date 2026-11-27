# Servidor de keys do Yapfast

A API que valida as keys de uma build do Yapfast feita com `YAPFAST_KEYS_URL`.
É um arquivo Python, `yapfast_keys.py`, que roda no seu PC sem instalar nada
além do Python 3.10+. O banco é SQLite (`keys.sqlite3`) e guarda **só o hash**
de cada key. O segredo de admin fica no Gerenciador de Credenciais do Windows
(`YapfastKeys/admin`), fora do app e fora do GitHub.

## Instalar (uma vez)

```powershell
python keys\yapfast_keys.py setup
```

Cria a pasta `%LOCALAPPDATA%\YapfastKeys` com o banco e guarda o segredo de
admin no Gerenciador de Credenciais. Nada disso vai pro Git.

## Ligar a API

```powershell
python keys\yapfast_keys.py serve
```

Ela escuta só no próprio PC:

- `127.0.0.1:8787`: a API pública, que o Yapfast chama (`/v1/validate` e `/v1/invite`);
- `127.0.0.1:8788`: a API de admin, que nunca sai do PC e exige o segredo.

Pra ligar junto com o Windows, escondida:

```powershell
python keys\yapfast_keys.py autostart on
```

## HTTPS público (Tailscale Funnel)

O Funnel dá um endereço `https://SEU-PC.SEU-TAILNET.ts.net` com certificado
válido e encaminha pra porta 8787, sem abrir porta no roteador.

1. Entre no Tailscale (ícone na bandeja, ou `tailscale up`).
2. Publique a API, uma vez só, que fica valendo depois de reiniciar:

   ```powershell
   tailscale funnel --bg 8787
   ```

   Na primeira vez, o Tailscale abre uma página pra você liberar o HTTPS e o
   Funnel na sua conta.
3. `tailscale funnel status` mostra o endereço `https://….ts.net`.

## Apontar a build pra API

No GitHub, em **Settings › Secrets and variables › Actions › Variables**, crie
`YAPFAST_KEYS_URL` com o endereço do Funnel (sem barra no fim). Toda build do
`overocai` a partir daí pede a key. Sem essa variável, a build abre sem key.

## Gerar e revogar keys

Com a API ligada:

```powershell
python keys\yapfast_keys.py create --note "Niko"
python keys\yapfast_keys.py list
python keys\yapfast_keys.py revoke k_1a2b3c
python keys\yapfast_keys.py revoke k_1a2b3c --cascade
python keys\yapfast_keys.py invites k_1a2b3c 1
```

- `create` mostra a key **uma vez só**: o servidor guarda só o hash. Por
  padrão ela pode gerar 1 convite (`--invites 0` tira).
- `revoke` funciona pelo id da lista ou pela própria key. O Yapfast de quem
  usa essa key trava em até 1 minuto. `--cascade` revoga também quem ela
  convidou.
- `invites` muda quantos convites uma key pode gerar. Uma key que veio de
  convite começa com 0.

## Convites

Em **Configurações › Minha conta**, quem tem convite clica em **Generate invite
code**. O código gerado é uma key nova, que vale pra 1 pessoa. Enquanto
ninguém usar o código, gerar de novo troca por outro. Depois que alguém usa,
o convite acaba.

## Se a API cair

Com o seu PC desligado, a API parada ou o Funnel fora do ar, todo mundo vê
**Yapfast is locked** com o motivo e **Try again**. O app também tenta de novo
sozinho a cada minuto.

## Testes

```powershell
python keys\test_yapfast_keys.py
```
