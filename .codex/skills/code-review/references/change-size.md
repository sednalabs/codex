# Change size

Unless a change is mechanical, keep the total diff at or below 800 changed
lines. For complex logic changes, target fewer than 500 changed lines.

When the diff is larger, explain whether it can be split into reviewable
stages and identify the smallest coherent stage to land first. Base that
recommendation on the actual diff, dependencies, and affected call sites.
