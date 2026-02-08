# mayl

protonmail email api via docker compose and tailscale

- per domain keys
- queued mail in case of high load or sending
  - 200/202 endpoints via query param?
- simple json blob POST `$endpoint:8080/email`
- configurable delays
  - default per domain: 1s

# mvp

- email api
- docker-compose with combination debian protonmail bridge image
- documented tailscale usage
