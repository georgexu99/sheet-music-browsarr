# MuseScore PDF worker — headful Chrome (nodriver) under Xvfb.
#
# The harvest technique (see HANDOFF.md) needs a *real* Chrome that clears the
# Cloudflare Turnstile and lets us screenshot each rendered score page. Headless
# is more detectable, so we run headful Chrome inside a virtual X server (Xvfb).
# This is the same shape as the old 21hsmw/flaresolverr:nodriver image that was
# confirmed to pass the challenge from inside Docker.
FROM python:3.12-slim-bookworm

ENV DEBIAN_FRONTEND=noninteractive

# System deps:
#   * google-chrome-stable — nodriver drives an installed Chrome (auto-detected
#     at /usr/bin/google-chrome).
#   * xvfb — virtual framebuffer so headful Chrome has a display.
#   * xauth — REQUIRED by `xvfb-run` to create the X authority file; without it
#     xvfb-run exits with "xauth command not found" and the container crash-loops
#     (it's a separate Debian package from xvfb, and a runtime — not build — dep,
#     so a successful image build doesn't catch its absence).
#   * fonts-* — so rendered pages aren't tofu boxes (sheet music has text:
#     titles, tempo marks, lyrics).
#   * dumb-init — proper PID 1 so SIGTERM reaps Chrome's process tree cleanly.
# Pin BRANDED google-chrome-stable to 149.0.7827.155 (from Google's deb pool,
# not the auto-updating `_current` link). Two independent reasons the pin is
# required, both learned the hard way in-container:
#   1. `_current` reached Chrome 150, which nodriver 0.50.3 (the newest release)
#      cannot launch reliably under Xvfb — it fails to start, then the DevTools
#      socket dies mid-run ("no close frame received or sent").
#   2. Cloudflare Turnstile does NOT clear for Chrome-for-Testing under Xvfb's
#      software renderer (CfT lacks the DRM/codec/branding a "real browser" has,
#      so it gets flagged). Only the BRANDED build clears the challenge here —
#      149 branded is exactly what cleared it on the first NAS deploy.
# 149.0.7827.155 is the newest branded build nodriver 0.50.3 drives cleanly AND
# that Turnstile accepts. If Google purges this deb from the pool, bump to the
# next pooled 149.x (or 150.x once a newer nodriver can drive it) and re-verify.
RUN apt-get update && apt-get install -y --no-install-recommends \
        wget gnupg ca-certificates xvfb xauth dumb-init \
        fonts-liberation fonts-dejavu-core fonts-noto-core fonts-noto-cjk \
    && wget -q -O /tmp/chrome.deb \
        https://dl.google.com/linux/chrome/deb/pool/main/g/google-chrome-stable/google-chrome-stable_149.0.7827.155-1_amd64.deb \
    && apt-get install -y --no-install-recommends /tmp/chrome.deb \
    && rm -f /tmp/chrome.deb \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY worker.py .

# Headful Chrome as root under Xvfb refuses its sandbox, and a default
# container's /dev/shm is only 64 MB (Chrome will crash rendering large pages).
# The worker forwards these into nodriver's browser_args (see get_browser).
# Compose still sets shm_size: 2gb as a belt-and-suspenders for any Chrome
# subsystem that ignores --disable-dev-shm-usage.
ENV CHROME_EXTRA_ARGS="--no-sandbox --disable-dev-shm-usage"
ENV PORT=8194
ENV WINDOW=1500,1000
# Unbuffered stdout/stderr so `log()` lines show up live in `docker logs` /
# Portainer instead of being held in Python's block buffer until exit.
ENV PYTHONUNBUFFERED=1
EXPOSE 8194

# Give Xvfb a screen taller than the 1400x2200@2x emulated viewport so page
# captures have backing pixels. dumb-init reaps zombies; xvfb-run -a picks a
# free display number and exports DISPLAY for the worker + Chrome.
ENTRYPOINT ["dumb-init", "--", "xvfb-run", "-a", "-s", "-screen 0 1920x2400x24", "python", "worker.py"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD python -c "import urllib.request,os; urllib.request.urlopen('http://127.0.0.1:'+os.environ.get('PORT','8194')+'/healthz').read()" || exit 1
