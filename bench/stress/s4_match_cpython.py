# S4 — pattern matching, pure CPython (structural `match`).
def weight(m):
    match m:
        case 0:
            return 7
        case 1 | 2 | 3:
            return 2
        case 11:
            return 5
        case x if x > 7:
            return 3
        case _:
            return 1


N = 20000000
print(sum(weight(k % 12) for k in range(N)))
