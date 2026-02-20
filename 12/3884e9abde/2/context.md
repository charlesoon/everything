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

### Prompt 6

commit

### Prompt 7

검색 결과를 가져올 때 500개씩 가져오도록 되어있는데 검색 결과가 총 몇개인지는 x results로 보여주고 목록을 500개씩 가져오면 좋겠어

### Prompt 8

구현해줄래?

### Prompt 9

a_desktop/ 하위의 파일이 500개만 있는게 아닐텐데 500으로 나와

### Prompt 10

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

