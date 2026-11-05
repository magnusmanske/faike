#!/bin/bash
# Restart the back-end bot (the `bot` Procfile entry) from the build-service image.
# The web service is restarted separately with `toolforge webservice buildservice restart`.
set -euo pipefail

toolforge jobs delete aimer-bot 2>/dev/null || true
rm -f /data/project/aimer/aimer-bot.out /data/project/aimer/aimer-bot.err

toolforge jobs run --mem 1Gi --continuous --mount=all \
	--image tool-aimer/tool-aimer:latest \
	--command bot \
	--filelog -o /data/project/aimer/aimer-bot.out -e /data/project/aimer/aimer-bot.err \
	aimer-bot
