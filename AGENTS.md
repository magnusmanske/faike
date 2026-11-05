## Hard rules

- **Do not add the AI attribution to commit messages.** 
- **`git commit` is pre-authorized; `git push` is not.** When work is complete and the diff is reviewable, you may run `git add <specific files>` + `git commit` without asking — this is durable authorization. **Never `git push`, `git push --force`, or otherwise publish commits to a remote without an explicit per-action request from the user.** Same for any other remote-publishing action (PR creation, branch deletion on origin, etc.). Use sensible commit groupings for larger changes.
- **`audits/`, `CLAUDE.md`, and `.claude/` are all gitignored.** Don't try to git-add them; treat audit files as durable scratch.
- This is a web-facing product, so **keep security in mind**.
- Always keep **code readability and long-term maintenance** in mind.
- Adhere to **SOLID and DRY principles**.
- Use **best practices** and **language standards**.
- **Keep the code simple** and elegant.
- **Write tests** where it makes sense.
- **Aim to keep code small** where possible.
- **Warn me if you think what I ask of you is a bad idea.** Or just window dressing, with no improvement of functionality, UX, or code readability. Be honest.
