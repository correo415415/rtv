#!/usr/bin/env bash
# Sincroniza ci/build.yml -> .github/workflows/build.yml y lo pushea.
#
# Por qué existe: el token de la GitHub App del asistente no tiene el permiso
# `workflows`, así que no puede tocar .github/workflows/. La fuente canónica
# del workflow vive en ci/build.yml (que sí puede editar en sus PRs); tras
# mergear un PR que lo cambie, ejecuta esto con tus credenciales:
#
#   git checkout main && git pull && bash ci/activar-workflow.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
if diff -q ci/build.yml .github/workflows/build.yml >/dev/null 2>&1; then
  echo "Ya está sincronizado; nada que hacer."
  exit 0
fi
mkdir -p .github/workflows
cp ci/build.yml .github/workflows/build.yml
git add .github/workflows/build.yml
git commit -m "ci: sincronizar workflow desde ci/build.yml"
git push
echo
echo "Listo. Pruébalo en Actions -> build -> Run workflow, o crea una release:"
echo "  git tag v0.2.0 && git push origin v0.2.0"
