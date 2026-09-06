# 生成エンジンのメモリと速度 — CUDA 実測(RTX 4090 24GB、2026-09-06)

[gen-design.md](gen-design.md) の §9 スパイク結果(Metal)に対する CUDA 側の対。
測定機は rtx4090(Ubuntu、CUDA 12.4、sd.cpp master-841-6b3edaa を `-DSD_CUDA=ON -DCMAKE_CUDA_ARCHITECTURES=89` でビルド)。

> ビルドの罠: Ubuntu の既定 g++ は 15.2 で CUDA 12.4 が受け付けない。
> `-DCMAKE_C_COMPILER=gcc-13 -DCMAKE_CXX_COMPILER=g++-13 -DCMAKE_CUDA_HOST_COMPILER=g++-13` が要る。

## 1. いちばん大事な発見 — `--vae-tiling` が無いと 1024² は落ちる

重み(7.8GB)は 24GB に楽に乗るのに、**VAE デコードの計算バッファだけで別途 6658 MiB を一括 cudaMalloc する**。
他の常駐(この機は ollama 8.7GB + fluent_gallery の ONNX 2.8GB)があると足りず、こう死ぬ:

```
[ERROR] ggml_backend_cuda_buffer_type_alloc_buffer: allocating 6658.00 MiB on device 0: cudaMalloc failed: out of memory
[ERROR] vae: failed to allocate the compute buffer
[ERROR] decode_first_stage failed for latent 1
```

サンプリングは成功しているので、**ジョブは「3秒で描けた絵を捨てて failed」になる**。
現行の `gen::start_server` は `--vae-tiling` を渡していないため、24GB 機でも 1024² が確実に失敗する。

## 2. 必要メモリの式(実測から)

```
必要VRAM = 重み + VAE計算バッファ
  重み           = 7.8GB (klein 4B: text_encoders 3.56GB + diffusion 4.10GB + vae 0.16GB)
  VAE計算バッファ = 6658 MiB × (W·H) / 1024²        ← 画素数に比例(実測で一致)
```

| 解像度 | VAE計算バッファ(実測/予測) |
|---|---|
| 1024² | 6658 MiB(OOMのエラーが吐いた実値) |
| 768²  | 3745 MiB(予測値と実測ピークが一致) |
| 512²  | 1664 MiB(同上) |

`--vae-tiling` を付けると第2項が **約1.8GBで頭打ち**(解像度に依らない)。
`--offload-to-cpu` を足すと第1項が **0.57GB** まで落ちる(重みをRAMに置き、必要な分だけVRAMに出し入れ)。

## 3. 速度(klein 4B / 4 steps / euler / txt_cfg 1.0 / seed 42、ジョブ投入から画像受け取りまで)

| モード | 512² | 768² | 1024² | 待機VRAM | 生成中ピーク |
|---|---|---|---|---|---|
| そのまま(全部VRAM) | **0.6s** | **1.3s** | 要14.5GB空き(この機ではOOM) | 7.8GB | 7.8+バッファ |
| `--vae-tiling` | 1.0s | 1.9s | **6.1s** | 7.8GB | 約9.7GB |
| `--vae-tiling --offload-to-cpu` | 1.9s | 2.9s | 8.8s | **0.57GB** | 約5.8GB |

- Metal(M3 Ultra)の 1024² 27秒/枚に対し、CUDA tiling で **6.1秒/枚 = 約4.4倍**。
- offload は tiling の 1.44 倍の時間で済む。**VRAMをほぼ使わずに 1024² が 8.8秒** なので、
  8GB級の非力なGPUでも「遅いが確実に動く」段として実用になる。
- タイリングの代償は小さい解像度ほど相対的に大きい(512²で1.7倍、1024²では逆にこれしか手が無い)。

## 3.5 VRAM を絞った機械での実測(他プロセスで VRAM を埋めて 8GB/6GB/4GB 機を再現)

`--vae-tiling --offload-to-cpu` で 1024²、空きVRAMだけを変えた:

| 空きVRAM | 1024² | sd-server の実使用 | 結果 |
|---|---|---|---|
| 24GB(素の実機) | 6.1s(tilingのみ) / 8.8s(offload併用) | 7.8GB / 5.8GB | |
| 8GB 相当 | **8.5s** | 5.2GB | 余裕あり |
| 6GB 相当 | **8.6s** | 5.2GB | 動く。速度は24GB機と変わらない |
| 4GB 相当 | 失敗 | - | `allocating 4101.40 MiB ... out of memory` |

**下限は「拡散モデル本体のサイズ」で決まる。** offload-to-cpu でも拡散重みだけは丸ごと VRAM に載せる必要があり、
klein 4B Q8_0 は 4101 MiB。よって **空き 6GB 未満の機械は量子化を落とすしか手が無い**(Q4_K_M なら約2.2GB)。
逆に 6GB さえあれば速度は 24GB 機とほぼ同じ(8.6s vs 8.8s)。VRAM は「動くか否か」を決めるが、
足りてさえいれば速度にはほとんど効かない ―― これは設定画面の説明にそのまま使える。

## 4. 動作モードの提案(3段)

機械の空きVRAMを見て自動で選べる。上から順に試して入る物を選ぶだけ:

| 段 | 条件 | 付ける引数 |
|---|---|---|
| 速い | 空きVRAM ≥ 重み + 6658MiB×(W·H)/1024² | (なし) |
| 節約 | 空きVRAM ≥ 重み + 2GB | `--vae-tiling` |
| 極小 | 空きVRAM ≥ 拡散モデル本体 + 1.5GB (klein Q8_0 なら 5.6GB) | `--vae-tiling --offload-to-cpu` |
| 足りない | 上のどれも入らない | 量子化を落とす → 解像度を落とす → 外部API(有料) の順で提案 |

## 5. 外部プロバイダ契約(`gen::generate_server`)の E2E 検証結果 — 穴なし

sd-server(CUDA)は下記を全て出し、`src/gen.rs` の期待と一致した。実機で job投入→poll→b64受領まで通っている。

| 使う口 | 実機 | 備考 |
|---|---|---|
| `GET /v1/models` | 200 `{"data":[{"id":"sd-cpp-local",...}]}` | `health()` はここを見る。生成は `/sdcpp/v1/*` と系統が違うが両方ある |
| `GET /sdcpp/v1/capabilities` | 200 | `defaults.sample_params` あり。`ref_images:true` / `lora:true` |
| `POST /sdcpp/v1/img_gen` | 200 `{"id":"job_...","status":"queued","poll_url":...}` | |
| `GET /sdcpp/v1/jobs/{id}` | `status` = `queued`/`completed`/`failed`/`cancelled` | gen.rs の受けと一致 |
| `POST /sdcpp/v1/jobs/{id}/cancel` | あり | |
| 結果 | `result.images[0].b64_json` | gen.rs の読み方と一致 |

## 6. 測り直す手順

```bash
cd ~/fluent_gallery && M=engine/models
./engine/bin/sd-server --diffusion-model $M/flux-2-klein-4b-Q8_0.gguf --vae $M/flux2-vae.safetensors \
  --llm $M/Qwen3-4B-Q4_K_M.gguf --lora-model-dir store/lora --diffusion-fa --vae-tiling \
  --listen-ip 0.0.0.0 --listen-port 8093
python3 /tmp/gen_bench.py   # capabilities を土台に steps/method/txt_cfg だけ上書きして 512/768/1024 を回す
```
