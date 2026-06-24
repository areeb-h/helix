let s = 0;
for (let i = 0; i < 10000000; i++) s = (s + i) % 1000000007;
console.log(s);
