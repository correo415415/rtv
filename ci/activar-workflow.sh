#!/usr/bin/env bash
# Activa el workflow de CI moviéndolo a .github/workflows/ (requiere permisos
# de humano: los tokens de la GitHub App del asistente no tienen el permiso
# `workflows` y GitHub rechaza que suban ficheros ahí).
#
# Uso (desde la raíz del repo, en la rama main ya mergeada):
#   bash ci/activar-workflow.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
mkdir -p .github/workflows
git mv ci/build.yml .github/workflows/build.yml
git commit -m "ci: activar workflow de builds multiplataforma"
git push
echo
echo "Listo. Pruébalo en Actions -> build -> Run workflow, o crea una release:"
echo "  git tag v0.2.0 && git push origin v0.2.0"
