data { int N; }
parameters { real theta; }
model {
  theta ~ normal(0, 1);
  target += N + supplied_from_r;
  target += another_missing;
}
