.PHONY: help build up down restart ps logs-ai logs-bot logs-printerd install-printerd-unit install-printerd-bin printerd-status printerd-restart printerd-logs

COMPOSE_FILE ?= deploy/docker-compose.yml
DEPLOY_DIR ?= /opt/printerbot

help:
	@echo "Targets:"
	@echo "  build                Build shared app image (contains ai-service + telegram-bot)"
	@echo "  up                   Start docker services in background"
	@echo "  down                 Stop docker services"
	@echo "  restart              Restart docker services"
	@echo "  ps                   Show docker services status"
	@echo "  logs-ai              Follow ai-service logs"
	@echo "  logs-bot             Follow telegram-bot logs"
	@echo "  install-printerd-bin Build and install printerd binary to $(DEPLOY_DIR)/bin"
	@echo "  install-printerd-unit Install systemd unit + env template for printerd"
	@echo "  printerd-status      Show printerd systemd status"
	@echo "  printerd-restart     Restart printerd systemd service"
	@echo "  printerd-logs        Follow printerd logs"
	@echo "  logs-printerd        Alias for printerd-logs"

build:
	docker compose -f $(COMPOSE_FILE) build ai-service

up:
	docker compose -f $(COMPOSE_FILE) up -d --build

down:
	docker compose -f $(COMPOSE_FILE) down

restart:
	docker compose -f $(COMPOSE_FILE) restart

ps:
	docker compose -f $(COMPOSE_FILE) ps

logs-ai:
	docker compose -f $(COMPOSE_FILE) logs -f ai-service

logs-bot:
	docker compose -f $(COMPOSE_FILE) logs -f telegram-bot

install-printerd-bin:
	cargo build --release -p printerd
	sudo install -Dm755 target/release/printerd $(DEPLOY_DIR)/bin/printerd

install-printerd-unit:
	sudo install -Dm644 deploy/systemd/printerd.service /etc/systemd/system/printerd.service
	sudo install -Dm644 deploy/systemd/printerd.env.example /etc/printerbot/printerd.env
	sudo systemctl daemon-reload
	sudo systemctl enable --now printerd

printerd-status:
	sudo systemctl status printerd --no-pager

printerd-restart:
	sudo systemctl restart printerd
	sudo systemctl status printerd --no-pager

printerd-logs:
	journalctl -u printerd -f

logs-printerd: printerd-logs
