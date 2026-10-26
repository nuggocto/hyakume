#!/usr/bin/env bash
# Fills server b's Scale workspace: 3 projects of 50 tasks and about 90 links,
# as a real team's tracker would have. Links follow fixed patterns rather
# than random picks, so every run makes the same data and no duplicates.
set -euo pipefail
cd "$(dirname "$0")/.."
scale() { HYAKUME_DATA_DIR=dev/data/profiles/alice ./target/debug/hyakume -w b/scale "$@" > /dev/null; }

subjects=(Login Billing Search Export Import Cache Queue Report Invoice Profile
  Settings Upload Webhook Audit Theme Email Sync Backup Alerts Metrics)
chores=("Fix crash in" "Add tests for" "Speed up" "Redesign" "Document"
  "Clean up" "Add logging to")
stages=(Backlog "To do" "In progress" Review Done)
for project in APP SRV OPS; do
  for i in $(seq 1 50); do
    title="${chores[$((i % 7))]} ${subjects[$(((i * 3) % 20))]}"
    owner=$([ $((i % 3)) -eq 0 ] && echo bob || echo alice)
    scale task create --project "$project" "$title" \
      --stage "${stages[$(((i * 7) % 5))]}" --assignee "$owner"
  done
done

for project in APP SRV OPS; do
  for i in $(seq 1 50); do
    # Chains of blockers and loose relations inside each project.
    if [ $((i % 4)) -eq 1 ] && [ $((i + 3)) -le 50 ]; then
      scale relation create "$project-$i" blocks "$project-$((i + 3))"
    fi
    if [ $((i % 6)) -eq 2 ] && [ $((i + 7)) -le 50 ]; then
      scale relation create "$project-$i" related_to "$project-$((i + 7))"
    fi
  done
done
for i in $(seq 1 50); do
  # Links between projects, the ones the overview bundles.
  if [ $((i % 5)) -eq 0 ]; then scale relation create "APP-$i" blocks "SRV-$i"; fi
  if [ $((i % 7)) -eq 3 ]; then scale relation create "SRV-$i" blocks "OPS-$((i + 1))"; fi
  if [ $((i % 9)) -eq 4 ]; then scale relation create "OPS-$i" related_to "APP-$((51 - i))"; fi
done
# One busy task, to see a lot light up at once.
for i in 2 4 6 8 10 12; do scale relation create SRV-1 blocks "APP-$i"; done
