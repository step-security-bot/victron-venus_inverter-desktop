# Repository infrastructure

GitHub configuration for `victron-venus/inverter-desktop` is maintained in
[terraform-github-victron](https://github.com/4alvit/terraform-github-victron).
This directory no longer contains an executable Pulumi project.

The canonical Terraform sources are:

- [`module.repos["inverter_desktop"]`](https://github.com/4alvit/terraform-github-victron/blob/main/main.tf)
  and the [`github_repo` module](https://github.com/4alvit/terraform-github-victron/blob/main/modules/github_repo/main.tf)
  for repository settings and security features.
- [`github_repository_ruleset.default`](https://github.com/4alvit/terraform-github-victron/blob/main/main.tf)
  for the Desktop default-branch review policy.
- [`release-standards.tf`](https://github.com/4alvit/terraform-github-victron/blob/main/release-standards.tf)
  for the required `CI gate`, external checks and release protections.

## Retired Pulumi source

The Pulumi project duplicated repository and branch-protection ownership and
still referenced obsolete CI job names. It had no current CI validation path.
Updating its dependencies would preserve a second, conflicting configuration
source, so its executable files and package manifest have been retired.

The complete previous source remains in Git at
[`b44f4091ec077f64be790a60075f8d4865e8873d:infra/`](https://github.com/victron-venus/inverter-desktop/tree/b44f4091ec077f64be790a60075f8d4865e8873d/infra).
Inspect or recover a historical file without activating the old project:

```sh
git show b44f4091ec077f64be790a60075f8d4865e8873d:infra/index.ts
```

This is a source-only retirement: it does not apply Terraform, delete live
infrastructure, transfer secrets or remove Pulumi state. GitHub deployment and
check metadata reviewed on 2026-09-29 showed no recent Pulumi activity. External
Pulumi stack and deployment configuration were not inspected or changed; their
owner must coordinate any operational retirement separately. Do not restore or
run the old project against the same resources without resolving that ownership.
