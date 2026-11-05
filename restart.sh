#!/bin/bash
# Restart the back-end bot (the `bot` Procfile entry) from the build-service image.
# The web service is restarted separately with `toolforge webservice buildservice restart`.
set -euo pipefail

toolforge jobs delete faike-bot 2>/dev/null || true
rm -f /data/project/faike/faike-bot.out /data/project/faike/faike-bot.err

toolforge jobs run --mem 2Gi --continuous --mount=all \
	--image tool-faike/tool-faike:latest \
	--command bot \
	--filelog -o /data/project/faike/faike-bot.out -e /data/project/faike/faike-bot.err \
	faike-bot
