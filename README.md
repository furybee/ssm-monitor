# ssm-monitor

A terminal UI to monitor AWS SSM-managed instances. Lists your fleet, surfaces CloudWatch alarms attached to each instance, lets you bookmark the ones you care about, and starts an SSM Session on the selected instance in one keystroke.

## Install

```bash
brew tap furybee/tap
brew install ssm-monitor
```

Or build from source:

```bash
cargo install --git https://github.com/furybee/ssm-monitor
```

## Requirements

- AWS credentials configured via the standard chain (`AWS_PROFILE`, `~/.aws/config`, SSO, IAM role, etc.)
- The [AWS CLI](https://docs.aws.amazon.com/cli/latest/userguide/install-cliv2.html) and the [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html) — needed only to start SSM sessions

### IAM permissions

```
ssm:DescribeInstanceInformation
ssm:StartSession
ssm:TerminateSession
ec2:DescribeInstances
cloudwatch:DescribeAlarms
```

## Usage

```bash
ssm-monitor
```

Press `?` at any time for the full keybinds. Quick reference:

| Key       | Action                                        |
|-----------|-----------------------------------------------|
| `↑`/`↓`   | Navigate the list                             |
| `←`/`→`   | Switch view (Bookmarks / All)                 |
| `Enter`   | Open instance details                         |
| `f`       | Find (filter by name or id)                   |
| `s`       | Cycle status filter                           |
| `a`       | Cycle alarm filter                            |
| `o`       | Cycle sort order                              |
| `b`       | Bookmark the selected instance                |
| `p`       | Switch AWS profile                            |
| `r`       | Refresh now (auto every 30s)                  |
| `c`       | (in detail view) Start an SSM session         |
| `?`       | Help                                          |
| `q`       | Quit                                          |

Bookmarks are persisted at `~/.config/ssm-monitor/favorites`.

## License

MIT. See [LICENSE](LICENSE).
