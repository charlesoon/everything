# Session Context

## User Prompts

### Prompt 1

아이템에 우클릭했을 때 finder에서 우클릭한것처럼 native context menu를 띄울 수 있을까?

### Prompt 2

그럼 option 1으로 구현해줄래?

### Prompt 3

실제 파일을 copy 하는 복사 기능도 menu에 넣어줘

### Prompt 4

Conduct a thorough, line-by-line code review with a critical perspective.

**Usage:**
```bash
/ln:codereview           # Review all uncommitted changes (default)
/ln:codereview all       # Review all uncommitted changes (staged + unstaged)
/ln:codereview unstaged  # Review only unstaged changes
```

## Review Scope

**Mode 1: All uncommitted changes (default)**
- Command: `git diff HEAD` (staged + unstaged)
- Reviews everything not yet committed

**Mode 2: Unstaged only**
- Command: `git diff` (...

### Prompt 5

모두 수정해줘

