/* RFC 1950 (zlib framing) + RFC 1951 (DEFLATE/INFLATE) + Adler-32 engine for
 * java.util.zip.Inflater and java.util.zip.Deflater. Self-contained C11 with
 * streaming byte-by-byte / 1-byte boundaries and no external dependencies. */
#include "jabs_runtime.h"

/* ---- Shared RFC 1951 constants ---- */

static const uint16_t zip_len_base[29] = {
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31,
    35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258
};

static const uint8_t zip_len_extra[29] = {
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2,
    3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0
};

static const uint16_t zip_dist_base[30] = {
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193,
    257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577
};

static const uint8_t zip_dist_extra[30] = {
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6,
    7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13
};

static const uint8_t zip_cl_order[19] = {
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15
};

static int64_t zip_pack_result(int32_t in_consumed, int32_t out_consumed, int finished, int flag63) {
    uint64_t res = ((uint64_t)(uint32_t)in_consumed & 0x7fffffffULL)
                 | (((uint64_t)(uint32_t)out_consumed & 0x7fffffffULL) << 31)
                 | ((uint64_t)(finished ? 1 : 0) << 62)
                 | ((uint64_t)(flag63 ? 1 : 0) << 63);
    return (int64_t)res;
}

/* ---- Inflater ---- */

#define ZIP_WIN_SIZE 32768u
#define ZIP_WIN_MASK 32767u
#define ZIP_MAX_BITS 15

typedef struct {
    int32_t max_len;
    uint16_t count[ZIP_MAX_BITS + 1];
    uint16_t symbol[288];
} zip_huff;

typedef enum {
    ZIP_INF_ZLIB_CMF,
    ZIP_INF_ZLIB_FLG,
    ZIP_INF_ZLIB_DICT0,
    ZIP_INF_ZLIB_DICT1,
    ZIP_INF_ZLIB_DICT2,
    ZIP_INF_ZLIB_DICT3,
    ZIP_INF_BLOCK_HEADER,
    ZIP_INF_STORED_LEN0,
    ZIP_INF_STORED_LEN1,
    ZIP_INF_STORED_LEN2,
    ZIP_INF_STORED_LEN3,
    ZIP_INF_STORED_DATA,
    ZIP_INF_DYN_HEADER,
    ZIP_INF_DYN_CL_LENS,
    ZIP_INF_DYN_TREE_SYM,
    ZIP_INF_DYN_TREE_REP,
    ZIP_INF_LIT_SYM,
    ZIP_INF_EMIT_LIT,
    ZIP_INF_LEN_EXTRA,
    ZIP_INF_DIST_SYM,
    ZIP_INF_DIST_EXTRA,
    ZIP_INF_COPY_MATCH,
    ZIP_INF_ZLIB_ADLER0,
    ZIP_INF_ZLIB_ADLER1,
    ZIP_INF_ZLIB_ADLER2,
    ZIP_INF_ZLIB_ADLER3,
    ZIP_INF_DONE,
    ZIP_INF_ERROR
} zip_inf_state;

typedef struct {
    int32_t nowrap;
    zip_inf_state state;
    uint32_t bitbuf;
    int32_t bitcnt;

    uint8_t zlib_cmf;
    uint32_t adler;
    uint32_t dict_adler;
    uint32_t trailer_adler;
    int32_t need_dict;
    int32_t reset_adler_on_data;
    int32_t block_started;

    int32_t bfinal;
    uint32_t stored_len;
    uint32_t stored_nlen;
    uint32_t stored_rem;

    /* Partial canonical Huffman decoding state across streaming boundaries. */
    int32_t huff_len;
    int32_t huff_code;
    int32_t huff_first;
    int32_t huff_index;

    uint32_t hlit;
    uint32_t hdist;
    uint32_t hclen;
    uint32_t dyn_idx;
    uint8_t cl_lens[19];
    uint8_t tree_lens[286 + 30];
    int32_t dyn_rep_sym;

    zip_huff cl_huff;
    zip_huff lit_huff;
    zip_huff dist_huff;

    uint8_t pending_lit;
    uint32_t len_idx;
    uint32_t dist_idx;
    uint32_t match_len;
    uint32_t match_dist;

    uint32_t win_pos;
    uint32_t win_avail;
    uint8_t window[ZIP_WIN_SIZE];

    const char *error_msg;
} zip_inflater;

static int zip_huff_build(zip_huff *h, const uint8_t *lengths, int n_syms, int require_complete) {
    uint16_t offs[ZIP_MAX_BITS + 1];
    int max_len = 0;

    memset(h->count, 0, sizeof(h->count));
    for (int i = 0; i < n_syms; i++) {
        uint8_t l = lengths[i];
        if (l > ZIP_MAX_BITS) return -1;
        h->count[l]++;
        if (l > max_len) max_len = l;
    }

    if (max_len == 0) {
        h->max_len = 0;
        return require_complete ? -1 : 0;
    }

    int left = 1;
    for (int len = 1; len <= ZIP_MAX_BITS; len++) {
        left <<= 1;
        left -= (int)h->count[len];
        if (left < 0) return -1;
    }
    if (left > 0 && (require_complete || max_len != 1)) {
        return -1;
    }

    h->max_len = max_len;
    offs[1] = 0;
    for (int len = 1; len < ZIP_MAX_BITS; len++) {
        offs[len + 1] = (uint16_t)(offs[len] + h->count[len]);
    }
    for (int sym = 0; sym < n_syms; sym++) {
        if (lengths[sym] != 0) {
            h->symbol[offs[lengths[sym]]++] = (uint16_t)sym;
        }
    }
    return 0;
}

static void zip_huff_build_fixed(zip_huff *lit_h, zip_huff *dist_h) {
    uint8_t lens[288];
    for (int i = 0; i <= 143; i++) lens[i] = 8;
    for (int i = 144; i <= 255; i++) lens[i] = 9;
    for (int i = 256; i <= 279; i++) lens[i] = 7;
    for (int i = 280; i <= 287; i++) lens[i] = 8;
    zip_huff_build(lit_h, lens, 288, 1);

    for (int i = 0; i < 32; i++) lens[i] = 5;
    zip_huff_build(dist_h, lens, 32, 1);
}

/* Pull bytes only until bitcnt >= n; since n <= 16 and bytes are added 8 bits at
 * a time, consuming n bits always leaves bitcnt in 0..7, never over-consuming. */
static int zip_inf_ensure_bits(zip_inflater *s, const uint8_t *in, int32_t in_len, int32_t *in_pos, int n) {
    while (s->bitcnt < n) {
        if (*in_pos >= in_len) return 0;
        s->bitbuf |= (uint32_t)in[(*in_pos)++] << s->bitcnt;
        s->bitcnt += 8;
    }
    return 1;
}

static uint32_t zip_inf_take_bits(zip_inflater *s, int n) {
    uint32_t val = s->bitbuf & ((1u << n) - 1u);
    s->bitbuf >>= n;
    s->bitcnt -= n;
    return val;
}

static void zip_inf_reset_huff_decode(zip_inflater *s) {
    s->huff_len = 0;
    s->huff_code = 0;
    s->huff_first = 0;
    s->huff_index = 0;
}

/* Returns: 1 = decoded symbol in *sym, 0 = need more input, -1 = invalid code. */
static int zip_inf_decode_huff(zip_inflater *s, const zip_huff *h, const uint8_t *in, int32_t in_len, int32_t *in_pos, int32_t *sym) {
    if (s->huff_len == 0) s->huff_len = 1;
    while (s->huff_len <= h->max_len) {
        if (!zip_inf_ensure_bits(s, in, in_len, in_pos, 1)) return 0;
        s->huff_code |= (int32_t)zip_inf_take_bits(s, 1);
        int32_t count = (int32_t)h->count[s->huff_len];
        if (s->huff_code - count < s->huff_first) {
            *sym = (int32_t)h->symbol[s->huff_index + (s->huff_code - s->huff_first)];
            zip_inf_reset_huff_decode(s);
            return 1;
        }
        s->huff_index += count;
        s->huff_first = (s->huff_first + count) << 1;
        s->huff_code <<= 1;
        s->huff_len++;
    }
    zip_inf_reset_huff_decode(s);
    return -1;
}

static void zip_inf_flush_adler(zip_inflater *s, const uint8_t *out, int32_t out_pos, int32_t *adler_flushed) {
    if (!s->nowrap && out_pos > *adler_flushed) {
        s->adler = jabs_adler32(s->adler, out + *adler_flushed, (size_t)(out_pos - *adler_flushed));
        *adler_flushed = out_pos;
    }
}

static int64_t zip_inf_fail(zip_inflater *s, jabs_object *inflater, int32_t in_pos, const uint8_t *out, int32_t out_pos, int32_t *adler_flushed, const char *msg) {
    zip_inf_flush_adler(s, out, out_pos, adler_flushed);
    s->state = ZIP_INF_ERROR;
    s->error_msg = msg;
    jabs_zip_set_inflater_consumed(inflater, in_pos, out_pos);
    jabs_zip_data_format_exception(msg);
    return 0;
}

static void zip_inf_win_push(zip_inflater *s, uint8_t b) {
    s->window[s->win_pos] = b;
    s->win_pos = (s->win_pos + 1u) & ZIP_WIN_MASK;
    if (s->win_avail < ZIP_WIN_SIZE) s->win_avail++;
}

int64_t jabs_zip_inflate_init(int32_t nowrap) {
    zip_inflater *s = (zip_inflater *)calloc(1, sizeof(zip_inflater));
    if (!s) {
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return 0;
    }
    s->nowrap = nowrap ? 1 : 0;
    jabs_zip_inflate_reset((int64_t)(intptr_t)s);
    return (int64_t)(intptr_t)s;
}

void jabs_zip_inflate_reset(int64_t stream) {
    zip_inflater *s = (zip_inflater *)(intptr_t)stream;
    if (!s) return;
    int32_t nowrap = s->nowrap;
    memset(s, 0, sizeof(*s));
    s->nowrap = nowrap;
    s->state = nowrap ? ZIP_INF_BLOCK_HEADER : ZIP_INF_ZLIB_CMF;
    s->adler = nowrap ? 0u : 1u;
}

void jabs_zip_inflate_end(int64_t stream) {
    zip_inflater *s = (zip_inflater *)(intptr_t)stream;
    free(s);
}

int32_t jabs_zip_inflate_adler(int64_t stream) {
    zip_inflater *s = (zip_inflater *)(intptr_t)stream;
    return s ? (int32_t)s->adler : 0;
}

void jabs_zip_inflate_set_dict(int64_t stream, const uint8_t *dict, int32_t len) {
    zip_inflater *s = (zip_inflater *)(intptr_t)stream;
    if (!s) return;
    if (len < 0 || (dict == NULL && len != 0)) {
        jabs_zip_illegal_argument("invalid dictionary");
        return;
    }
    if (!s->nowrap) {
        if (!s->need_dict) {
            jabs_zip_illegal_argument("dictionary not needed");
            return;
        }
        uint32_t d_adler = jabs_adler32(1u, dict, (size_t)len);
        if (d_adler != s->adler) {
            jabs_zip_illegal_argument("dictionary mismatch");
            return;
        }
        /* zlib keeps the dictionary Adler-32 visible via getAdler() until the next inflate() call. */
        s->reset_adler_on_data = 1;
        s->need_dict = 0;
    } else {
        if (s->state != ZIP_INF_BLOCK_HEADER || s->block_started) {
            jabs_zip_illegal_argument("dictionary not allowed after data");
            return;
        }
    }

    size_t copy_len = (size_t)len;
    const uint8_t *src = dict;
    if (copy_len > ZIP_WIN_SIZE) {
        src += copy_len - ZIP_WIN_SIZE;
        copy_len = ZIP_WIN_SIZE;
    }
    for (size_t i = 0; i < copy_len; i++) {
        zip_inf_win_push(s, src[i]);
    }
}

int64_t jabs_zip_inflate(jabs_object *inflater, int64_t stream, const uint8_t *in, int32_t in_len, uint8_t *out, int32_t out_len) {
    zip_inflater *s = (zip_inflater *)(intptr_t)stream;
    if (!s) return 0;
    if (in_len < 0) in_len = 0;
    if (out_len < 0) out_len = 0;

    if (s->state == ZIP_INF_ERROR) {
        jabs_zip_set_inflater_consumed(inflater, 0, 0);
        jabs_zip_data_format_exception(s->error_msg ? s->error_msg : "invalid deflate data");
        return 0;
    }
    if (s->state == ZIP_INF_DONE) {
        return zip_pack_result(0, 0, 1, 0);
    }
    if (s->need_dict) {
        return zip_pack_result(0, 0, 0, 1);
    }
    if (s->reset_adler_on_data) {
        s->adler = s->nowrap ? 0u : 1u;
        s->reset_adler_on_data = 0;
    }

    int32_t in_pos = 0;
    int32_t out_pos = 0;
    int32_t adler_flushed = 0;

    for (;;) {
        switch (s->state) {
        case ZIP_INF_ZLIB_CMF:
            if (in_pos >= in_len) goto done_loop;
            s->zlib_cmf = in[in_pos++];
            if ((s->zlib_cmf & 0x0fu) != 8u) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "unknown compression method");
            }
            if ((s->zlib_cmf >> 4) > 7u) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid window size");
            }
            s->state = ZIP_INF_ZLIB_FLG;
            break;

        case ZIP_INF_ZLIB_FLG: {
            if (in_pos >= in_len) goto done_loop;
            uint8_t flg = in[in_pos++];
            if ((((uint32_t)s->zlib_cmf << 8) | flg) % 31u != 0u) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "incorrect header check");
            }
            if (flg & 0x20u) {
                s->dict_adler = 0;
                s->state = ZIP_INF_ZLIB_DICT0;
            } else {
                s->adler = 1u;
                s->state = ZIP_INF_BLOCK_HEADER;
            }
            break;
        }

        case ZIP_INF_ZLIB_DICT0:
        case ZIP_INF_ZLIB_DICT1:
        case ZIP_INF_ZLIB_DICT2:
        case ZIP_INF_ZLIB_DICT3:
            if (in_pos >= in_len) goto done_loop;
            s->dict_adler = (s->dict_adler << 8) | in[in_pos++];
            if (s->state == ZIP_INF_ZLIB_DICT3) {
                s->adler = s->dict_adler;
                s->need_dict = 1;
                s->state = ZIP_INF_BLOCK_HEADER;
                goto done_loop;
            }
            s->state = (zip_inf_state)(s->state + 1);
            break;

        case ZIP_INF_BLOCK_HEADER: {
            if (!zip_inf_ensure_bits(s, in, in_len, &in_pos, 3)) goto done_loop;
            s->block_started = 1;
            s->bfinal = (int32_t)zip_inf_take_bits(s, 1);
            uint32_t btype = zip_inf_take_bits(s, 2);
            if (btype == 0u) {
                /* Stored block: discard 0..7 padding bits to byte boundary. */
                s->bitbuf = 0;
                s->bitcnt = 0;
                s->state = ZIP_INF_STORED_LEN0;
            } else if (btype == 1u) {
                zip_huff_build_fixed(&s->lit_huff, &s->dist_huff);
                zip_inf_reset_huff_decode(s);
                s->state = ZIP_INF_LIT_SYM;
            } else if (btype == 2u) {
                s->state = ZIP_INF_DYN_HEADER;
            } else {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid block type");
            }
            break;
        }

        case ZIP_INF_STORED_LEN0:
            if (in_pos >= in_len) goto done_loop;
            s->stored_len = in[in_pos++];
            s->state = ZIP_INF_STORED_LEN1;
            break;

        case ZIP_INF_STORED_LEN1:
            if (in_pos >= in_len) goto done_loop;
            s->stored_len |= (uint32_t)in[in_pos++] << 8;
            s->state = ZIP_INF_STORED_LEN2;
            break;

        case ZIP_INF_STORED_LEN2:
            if (in_pos >= in_len) goto done_loop;
            s->stored_nlen = in[in_pos++];
            s->state = ZIP_INF_STORED_LEN3;
            break;

        case ZIP_INF_STORED_LEN3:
            if (in_pos >= in_len) goto done_loop;
            s->stored_nlen |= (uint32_t)in[in_pos++] << 8;
            if ((s->stored_len ^ 0xffffu) != s->stored_nlen) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid stored block lengths");
            }
            s->stored_rem = s->stored_len;
            if (s->stored_rem == 0u) {
                s->state = s->bfinal ? (s->nowrap ? ZIP_INF_DONE : ZIP_INF_ZLIB_ADLER0) : ZIP_INF_BLOCK_HEADER;
            } else {
                s->state = ZIP_INF_STORED_DATA;
            }
            break;

        case ZIP_INF_STORED_DATA: {
            if (in_pos >= in_len || out_pos >= out_len) goto done_loop;
            uint32_t avail_in = (uint32_t)(in_len - in_pos);
            uint32_t avail_out = (uint32_t)(out_len - out_pos);
            uint32_t n = s->stored_rem;
            if (avail_in < n) n = avail_in;
            if (avail_out < n) n = avail_out;
            memcpy(out + out_pos, in + in_pos, n);
            for (uint32_t i = 0; i < n; i++) {
                zip_inf_win_push(s, in[in_pos + (int32_t)i]);
            }
            in_pos += (int32_t)n;
            out_pos += (int32_t)n;
            s->stored_rem -= n;
            if (s->stored_rem == 0u) {
                s->state = s->bfinal ? (s->nowrap ? ZIP_INF_DONE : ZIP_INF_ZLIB_ADLER0) : ZIP_INF_BLOCK_HEADER;
            }
            break;
        }

        case ZIP_INF_DYN_HEADER:
            if (!zip_inf_ensure_bits(s, in, in_len, &in_pos, 14)) goto done_loop;
            s->hlit = zip_inf_take_bits(s, 5) + 257u;
            s->hdist = zip_inf_take_bits(s, 5) + 1u;
            s->hclen = zip_inf_take_bits(s, 4) + 4u;
            if (s->hlit > 286u) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid literal/lengths set");
            }
            if (s->hdist > 30u) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid distances set");
            }
            memset(s->cl_lens, 0, sizeof(s->cl_lens));
            s->dyn_idx = 0;
            s->state = ZIP_INF_DYN_CL_LENS;
            break;

        case ZIP_INF_DYN_CL_LENS:
            while (s->dyn_idx < s->hclen) {
                if (!zip_inf_ensure_bits(s, in, in_len, &in_pos, 3)) goto done_loop;
                s->cl_lens[zip_cl_order[s->dyn_idx++]] = (uint8_t)zip_inf_take_bits(s, 3);
            }
            if (zip_huff_build(&s->cl_huff, s->cl_lens, 19, 1) != 0) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid code lengths set");
            }
            s->dyn_idx = 0;
            zip_inf_reset_huff_decode(s);
            s->state = ZIP_INF_DYN_TREE_SYM;
            break;

        case ZIP_INF_DYN_TREE_SYM: {
            uint32_t n_total = s->hlit + s->hdist;
            while (s->dyn_idx < n_total) {
                int32_t sym = 0;
                int rc = zip_inf_decode_huff(s, &s->cl_huff, in, in_len, &in_pos, &sym);
                if (rc == 0) goto done_loop;
                if (rc < 0) {
                    return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid code lengths set");
                }
                if (sym < 16) {
                    s->tree_lens[s->dyn_idx++] = (uint8_t)sym;
                } else {
                    if (sym == 16 && s->dyn_idx == 0) {
                        return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid bit length repeat");
                    }
                    s->dyn_rep_sym = sym;
                    s->state = ZIP_INF_DYN_TREE_REP;
                    goto dyn_rep_label;
                }
            }
            if (s->tree_lens[256] == 0) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid literal/lengths set");
            }
            if (zip_huff_build(&s->lit_huff, s->tree_lens, (int)s->hlit, 0) != 0) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid literal/lengths set");
            }
            if (zip_huff_build(&s->dist_huff, s->tree_lens + s->hlit, (int)s->hdist, 0) != 0) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid distances set");
            }
            zip_inf_reset_huff_decode(s);
            s->state = ZIP_INF_LIT_SYM;
            break;
        }

        dyn_rep_label:
        case ZIP_INF_DYN_TREE_REP: {
            int nbits = (s->dyn_rep_sym == 16) ? 2 : (s->dyn_rep_sym == 17 ? 3 : 7);
            uint32_t base = (s->dyn_rep_sym == 18) ? 11u : 3u;
            uint8_t fill = (s->dyn_rep_sym == 16) ? s->tree_lens[s->dyn_idx - 1u] : 0u;
            if (!zip_inf_ensure_bits(s, in, in_len, &in_pos, nbits)) goto done_loop;
            uint32_t rep = base + zip_inf_take_bits(s, nbits);
            if (s->dyn_idx + rep > s->hlit + s->hdist) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid bit length repeat");
            }
            for (uint32_t i = 0; i < rep; i++) {
                s->tree_lens[s->dyn_idx++] = fill;
            }
            s->state = ZIP_INF_DYN_TREE_SYM;
            break;
        }

        case ZIP_INF_LIT_SYM: {
            int32_t sym = 0;
            int rc = zip_inf_decode_huff(s, &s->lit_huff, in, in_len, &in_pos, &sym);
            if (rc == 0) goto done_loop;
            if (rc < 0 || sym >= 286) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid literal/length code");
            }
            if (sym < 256) {
                if (out_pos < out_len) {
                    uint8_t b = (uint8_t)sym;
                    out[out_pos++] = b;
                    zip_inf_win_push(s, b);
                } else {
                    s->pending_lit = (uint8_t)sym;
                    s->state = ZIP_INF_EMIT_LIT;
                    goto done_loop;
                }
            } else if (sym == 256) {
                if (s->bfinal) {
                    /* Byte-align at end of final block; bitcnt is in 0..7 so 0 bytes are rewound. */
                    s->bitbuf = 0;
                    s->bitcnt = 0;
                    s->state = s->nowrap ? ZIP_INF_DONE : ZIP_INF_ZLIB_ADLER0;
                } else {
                    s->state = ZIP_INF_BLOCK_HEADER;
                }
            } else {
                s->len_idx = (uint32_t)(sym - 257);
                s->state = ZIP_INF_LEN_EXTRA;
            }
            break;
        }

        case ZIP_INF_EMIT_LIT:
            if (out_pos >= out_len) goto done_loop;
            out[out_pos++] = s->pending_lit;
            zip_inf_win_push(s, s->pending_lit);
            s->state = ZIP_INF_LIT_SYM;
            break;

        case ZIP_INF_LEN_EXTRA: {
            int nbits = (int)zip_len_extra[s->len_idx];
            if (nbits > 0 && !zip_inf_ensure_bits(s, in, in_len, &in_pos, nbits)) goto done_loop;
            s->match_len = zip_len_base[s->len_idx] + (nbits > 0 ? zip_inf_take_bits(s, nbits) : 0u);
            zip_inf_reset_huff_decode(s);
            s->state = ZIP_INF_DIST_SYM;
            break;
        }

        case ZIP_INF_DIST_SYM: {
            int32_t sym = 0;
            int rc = zip_inf_decode_huff(s, &s->dist_huff, in, in_len, &in_pos, &sym);
            if (rc == 0) goto done_loop;
            if (rc < 0 || sym >= 30) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid distance code");
            }
            s->dist_idx = (uint32_t)sym;
            s->state = ZIP_INF_DIST_EXTRA;
            break;
        }

        case ZIP_INF_DIST_EXTRA: {
            int nbits = (int)zip_dist_extra[s->dist_idx];
            if (nbits > 0 && !zip_inf_ensure_bits(s, in, in_len, &in_pos, nbits)) goto done_loop;
            s->match_dist = zip_dist_base[s->dist_idx] + (nbits > 0 ? zip_inf_take_bits(s, nbits) : 0u);
            if (s->match_dist == 0u || s->match_dist > s->win_avail) {
                return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "invalid distance too far back");
            }
            s->state = ZIP_INF_COPY_MATCH;
            break;
        }

        case ZIP_INF_COPY_MATCH:
            while (s->match_len > 0u) {
                if (out_pos >= out_len) goto done_loop;
                uint8_t b = s->window[(s->win_pos - s->match_dist) & ZIP_WIN_MASK];
                out[out_pos++] = b;
                zip_inf_win_push(s, b);
                s->match_len--;
            }
            s->state = ZIP_INF_LIT_SYM;
            break;

        case ZIP_INF_ZLIB_ADLER0:
        case ZIP_INF_ZLIB_ADLER1:
        case ZIP_INF_ZLIB_ADLER2:
        case ZIP_INF_ZLIB_ADLER3:
            if (in_pos >= in_len) goto done_loop;
            s->trailer_adler = (s->trailer_adler << 8) | in[in_pos++];
            if (s->state == ZIP_INF_ZLIB_ADLER3) {
                zip_inf_flush_adler(s, out, out_pos, &adler_flushed);
                if (s->trailer_adler != s->adler) {
                    return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, "incorrect data check");
                }
                s->state = ZIP_INF_DONE;
                return zip_pack_result(in_pos, out_pos, 1, 0);
            }
            s->state = (zip_inf_state)(s->state + 1);
            break;

        case ZIP_INF_DONE:
            goto done_loop;

        case ZIP_INF_ERROR:
            return zip_inf_fail(s, inflater, in_pos, out, out_pos, &adler_flushed, s->error_msg ? s->error_msg : "invalid deflate data");
        }
    }

done_loop:
    zip_inf_flush_adler(s, out, out_pos, &adler_flushed);
    return zip_pack_result(in_pos, out_pos, s->state == ZIP_INF_DONE, s->need_dict);
}

/* ---- Deflater ---- */

#define ZIP_DEF_NO_FLUSH   0
#define ZIP_DEF_SYNC_FLUSH 2
#define ZIP_DEF_FULL_FLUSH 3
#define ZIP_DEF_FINISH     4

#define ZIP_STRAT_DEFAULT  0
#define ZIP_STRAT_FILTERED 1
#define ZIP_STRAT_HUFFMAN  2

#define ZIP_DEF_WIN_CAP    65536
#define ZIP_DEF_MAX_LOOK   262
#define ZIP_DEF_MAX_MATCH  258
#define ZIP_DEF_MAX_TOKENS 16384
#define ZIP_DEF_HASH_SIZE  32768
#define ZIP_DEF_HASH_MASK  32767u

typedef struct {
    uint16_t dist;     /* 0 = literal byte, >0 = match distance */
    uint16_t lit_len;  /* literal 0..255, or match length 3..258 */
} zip_token;

typedef struct {
    int32_t level;
    int32_t strategy;
    int32_t nowrap;

    int32_t header_written;
    int32_t has_dict;
    int32_t finished;
    int32_t oom;
    int32_t last_flush;
    uint32_t adler;

    uint32_t bitbuf;
    int32_t bitcnt;

    uint8_t *out_buf;
    size_t out_cap;
    size_t out_head;
    size_t out_tail;

    int32_t strstart;
    int32_t block_start;
    int32_t lookahead;
    int32_t prev_avail;
    int32_t prev_len;
    int32_t prev_dist;

    int32_t token_count;
    zip_token tokens[ZIP_DEF_MAX_TOKENS];

    uint8_t window[ZIP_DEF_WIN_CAP];
    int32_t head[ZIP_DEF_HASH_SIZE];
    int32_t prev[ZIP_WIN_SIZE];
} zip_deflater;

static uint16_t zip_bitrev(uint16_t code, int len) {
    uint16_t res = 0;
    for (int i = 0; i < len; i++) {
        res = (uint16_t)((res << 1) | (code & 1u));
        code >>= 1;
    }
    return res;
}

static int zip_out_reserve(zip_deflater *s, size_t need) {
    if (s->oom) return 0;
    if (s->out_head == s->out_tail) {
        s->out_head = 0;
        s->out_tail = 0;
    }
    if (need > SIZE_MAX - s->out_tail) {
        s->oom = 1;
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return 0;
    }
    if (s->out_tail + need <= s->out_cap) return 1;
    if (s->out_head > 0) {
        size_t rem = s->out_tail - s->out_head;
        memmove(s->out_buf, s->out_buf + s->out_head, rem);
        s->out_head = 0;
        s->out_tail = rem;
        if (s->out_tail + need <= s->out_cap) return 1;
    }
    size_t target = s->out_tail + need;
    size_t new_cap = s->out_cap ? s->out_cap : 16384u;
    while (new_cap < target) {
        if (new_cap > SIZE_MAX / 2u) {
            new_cap = target;
            break;
        }
        new_cap *= 2u;
    }
    uint8_t *p = (uint8_t *)realloc(s->out_buf, new_cap);
    if (!p) {
        s->oom = 1;
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return 0;
    }
    s->out_buf = p;
    s->out_cap = new_cap;
    return 1;
}

static void zip_out_byte(zip_deflater *s, uint8_t b) {
    if (!zip_out_reserve(s, 1u)) return;
    s->out_buf[s->out_tail++] = b;
}

static void zip_out_bytes(zip_deflater *s, const uint8_t *p, size_t n) {
    if (n == 0 || !zip_out_reserve(s, n)) return;
    memcpy(s->out_buf + s->out_tail, p, n);
    s->out_tail += n;
}

static void zip_put_bits(zip_deflater *s, uint32_t bits, int n) {
    if (s->oom) return;
    s->bitbuf |= bits << s->bitcnt;
    s->bitcnt += n;
    while (s->bitcnt >= 8) {
        zip_out_byte(s, (uint8_t)(s->bitbuf & 0xffu));
        if (s->oom) return;
        s->bitbuf >>= 8;
        s->bitcnt -= 8;
    }
}

static void zip_flush_bits(zip_deflater *s) {
    if (s->oom) return;
    if (s->bitcnt > 0) {
        zip_out_byte(s, (uint8_t)(s->bitbuf & 0xffu));
        if (s->oom) return;
        s->bitbuf = 0;
        s->bitcnt = 0;
    }
}

static void zip_out_u16_le(zip_deflater *s, uint16_t v) {
    uint8_t b[2] = {(uint8_t)(v & 0xffu), (uint8_t)(v >> 8)};
    zip_out_bytes(s, b, 2u);
}

static void zip_out_u32_be(zip_deflater *s, uint32_t v) {
    uint8_t b[4] = {
        (uint8_t)(v >> 24), (uint8_t)((v >> 16) & 0xffu),
        (uint8_t)((v >> 8) & 0xffu), (uint8_t)(v & 0xffu)
    };
    zip_out_bytes(s, b, 4u);
}

static void zip_write_zlib_header(zip_deflater *s) {
    if (s->oom || s->header_written) return;
    if (s->nowrap) {
        s->header_written = 1;
        return;
    }
    uint8_t cmf = 0x78u;
    uint32_t flevel = (s->level <= 1) ? 0u : (s->level <= 5 ? 1u : (s->level == 6 ? 2u : 3u));
    uint32_t flg = (flevel << 6) | (s->has_dict ? 0x20u : 0u);
    uint32_t hdr = ((uint32_t)cmf << 8) | flg;
    flg += (31u - (hdr % 31u)) % 31u;
    zip_out_byte(s, cmf);
    zip_out_byte(s, (uint8_t)flg);
    if (s->has_dict) {
        zip_out_u32_be(s, s->adler);
        if (s->oom) return;
        s->adler = 1u;
    }
    if (!s->oom) s->header_written = 1;
}

static int zip_len_code_idx(int len) {
    if (len == 258) return 28;
    for (int i = 0; i < 28; i++) {
        if (len < (int)zip_len_base[i + 1]) return i;
    }
    return 27;
}

static int zip_dist_code_idx(int dist) {
    uint32_t v = (uint32_t)(dist - 1);
    int b = 0;
    while (v >= 4u) {
        v >>= 1;
        b += 2;
    }
    return b + (int)v;
}

static uint32_t zip_fixed_lit_len(int sym) {
    if (sym <= 143) return 8u;
    if (sym <= 255) return 9u;
    if (sym <= 279) return 7u;
    return 8u;
}

static uint16_t zip_fixed_lit_code(int sym) {
    uint16_t code;
    int len;
    if (sym <= 143) { code = (uint16_t)(0x30 + sym); len = 8; }
    else if (sym <= 255) { code = (uint16_t)(0x190 + (sym - 144)); len = 9; }
    else if (sym <= 279) { code = (uint16_t)(sym - 256); len = 7; }
    else { code = (uint16_t)(0xc0 + (sym - 280)); len = 8; }
    return zip_bitrev(code, len);
}

static uint16_t zip_fixed_dist_code(int sym) {
    return zip_bitrev((uint16_t)sym, 5);
}

/* Min-heap of ((uint64_t)freq << 16) | node_id for building Huffman trees. */
static void zip_heap_push(uint64_t *heap, int *n, uint64_t val) {
    int i = (*n)++;
    while (i > 0) {
        int p = (i - 1) >> 1;
        if (heap[p] <= val) break;
        heap[i] = heap[p];
        i = p;
    }
    heap[i] = val;
}

static uint64_t zip_heap_pop(uint64_t *heap, int *n) {
    uint64_t top = heap[0];
    uint64_t last = heap[--(*n)];
    int i = 0;
    for (;;) {
        int l = i * 2 + 1;
        if (l >= *n) break;
        int r = l + 1;
        int c = (r < *n && heap[r] < heap[l]) ? r : l;
        if (last <= heap[c]) break;
        heap[i] = heap[c];
        i = c;
    }
    if (*n > 0) heap[i] = last;
    return top;
}

/* Build length-limited canonical Huffman lengths (<= max_bits) from freqs[]. */
static void zip_build_tree_lengths(const uint32_t *freqs, int n_syms, int max_bits, uint8_t *lens, int force_two) {
    int active[286];
    int n_act = 0;
    memset(lens, 0, (size_t)n_syms);

    for (int i = 0; i < n_syms; i++) {
        if (freqs[i] > 0) active[n_act++] = i;
    }
    if (n_act == 0) return;
    if (n_act == 1) {
        lens[active[0]] = 1;
        if (force_two && n_syms >= 2) {
            lens[active[0] == 0 ? 1 : 0] = 1;
        }
        return;
    }

    uint64_t heap[572];
    int heap_n = 0;
    int16_t parent[572];
    for (int i = 0; i < 572; i++) parent[i] = -1;

    for (int i = 0; i < n_act; i++) {
        zip_heap_push(heap, &heap_n, ((uint64_t)freqs[active[i]] << 16) | (uint64_t)i);
    }
    int next_node = n_act;
    while (heap_n > 1) {
        uint64_t a = zip_heap_pop(heap, &heap_n);
        uint64_t b = zip_heap_pop(heap, &heap_n);
        int p = next_node++;
        parent[a & 0xffffu] = (int16_t)p;
        parent[b & 0xffffu] = (int16_t)p;
        uint64_t sum = (a >> 16) + (b >> 16);
        zip_heap_push(heap, &heap_n, (sum << 16) | (uint64_t)p);
    }

    uint16_t bl_count[16] = {0};
    for (int i = 0; i < n_act; i++) {
        int d = 0;
        for (int cur = i; parent[cur] >= 0; cur = parent[cur]) d++;
        if (d > max_bits) d = max_bits;
        bl_count[d]++;
    }

    /* Enforce Kraft equality by shifting leaves down when depths exceeded max_bits. */
    uint32_t target = 1u << max_bits;
    uint32_t kraft = 0;
    for (int b = 1; b <= max_bits; b++) {
        kraft += (uint32_t)bl_count[b] << (max_bits - b);
    }
    while (kraft > target) {
        int b = max_bits - 1;
        while (b > 0 && bl_count[b] == 0) b--;
        bl_count[b]--;
        bl_count[b + 1] += 2;
        bl_count[max_bits]--;
        kraft--;
    }

    /* Sort active symbols by descending frequency so shortest lengths go to most frequent. */
    for (int i = 1; i < n_act; i++) {
        int key = active[i];
        uint32_t kf = freqs[key];
        int j = i - 1;
        while (j >= 0 && freqs[active[j]] < kf) {
            active[j + 1] = active[j];
            j--;
        }
        active[j + 1] = key;
    }

    int idx = 0;
    for (int b = 1; b <= max_bits; b++) {
        for (uint16_t c = 0; c < bl_count[b]; c++) {
            lens[active[idx++]] = (uint8_t)b;
        }
    }
}

static void zip_gen_codes(const uint8_t *lens, int n_syms, uint16_t *codes) {
    uint16_t bl_count[16] = {0};
    for (int i = 0; i < n_syms; i++) {
        if (lens[i] > 0) bl_count[lens[i]]++;
    }
    uint16_t next_code[16];
    uint16_t code = 0;
    bl_count[0] = 0;
    for (int b = 1; b <= 15; b++) {
        code = (uint16_t)((code + bl_count[b - 1]) << 1);
        next_code[b] = code;
    }
    for (int i = 0; i < n_syms; i++) {
        int l = lens[i];
        codes[i] = l ? zip_bitrev(next_code[l]++, l) : 0;
    }
}

static int zip_rle_lengths(const uint8_t *lens, int n, uint8_t *rle_sym, uint8_t *rle_extra, uint32_t *cl_freq) {
    int out_n = 0;
    int i = 0;
    memset(cl_freq, 0, sizeof(uint32_t) * 19u);
    while (i < n) {
        uint8_t cur = lens[i];
        int run = 1;
        while (i + run < n && lens[i + run] == cur) run++;
        if (cur == 0) {
            int rem = run;
            while (rem >= 11) {
                int k = rem > 138 ? 138 : rem;
                rle_sym[out_n] = 18;
                rle_extra[out_n++] = (uint8_t)(k - 11);
                cl_freq[18]++;
                rem -= k;
            }
            if (rem >= 3) {
                rle_sym[out_n] = 17;
                rle_extra[out_n++] = (uint8_t)(rem - 3);
                cl_freq[17]++;
                rem = 0;
            }
            while (rem-- > 0) {
                rle_sym[out_n] = 0;
                rle_extra[out_n++] = 0;
                cl_freq[0]++;
            }
        } else {
            rle_sym[out_n] = cur;
            rle_extra[out_n++] = 0;
            cl_freq[cur]++;
            int rem = run - 1;
            while (rem >= 3) {
                int k = rem > 6 ? 6 : rem;
                rle_sym[out_n] = 16;
                rle_extra[out_n++] = (uint8_t)(k - 3);
                cl_freq[16]++;
                rem -= k;
            }
            while (rem-- > 0) {
                rle_sym[out_n] = cur;
                rle_extra[out_n++] = 0;
                cl_freq[cur]++;
            }
        }
        i += run;
    }
    return out_n;
}

static void zip_emit_tokens(zip_deflater *s, const uint16_t *lit_codes, const uint8_t *lit_lens, const uint16_t *dist_codes, const uint8_t *dist_lens) {
    for (int i = 0; i < s->token_count; i++) {
        zip_token t = s->tokens[i];
        if (t.dist == 0) {
            int sym = (int)t.lit_len;
            zip_put_bits(s, lit_codes[sym], (int)lit_lens[sym]);
        } else {
            int l_idx = zip_len_code_idx((int)t.lit_len);
            int l_sym = 257 + l_idx;
            zip_put_bits(s, lit_codes[l_sym], (int)lit_lens[l_sym]);
            if (zip_len_extra[l_idx] > 0) {
                zip_put_bits(s, (uint32_t)(t.lit_len - zip_len_base[l_idx]), (int)zip_len_extra[l_idx]);
            }
            int d_idx = zip_dist_code_idx((int)t.dist);
            zip_put_bits(s, dist_codes[d_idx], (int)dist_lens[d_idx]);
            if (zip_dist_extra[d_idx] > 0) {
                zip_put_bits(s, (uint32_t)(t.dist - zip_dist_base[d_idx]), (int)zip_dist_extra[d_idx]);
            }
        }
    }
    zip_put_bits(s, lit_codes[256], (int)lit_lens[256]);
}

static void zip_flush_block(zip_deflater *s, int bfinal) {
    if (s->oom) return;
    zip_write_zlib_header(s);
    if (s->oom) return;
    int32_t raw_len = s->strstart - s->block_start;

    if (s->level == 0) {
        zip_put_bits(s, (uint32_t)(bfinal ? 1u : 0u), 3);
        zip_flush_bits(s);
        zip_out_u16_le(s, (uint16_t)raw_len);
        zip_out_u16_le(s, (uint16_t)(~(uint32_t)raw_len));
        if (raw_len > 0) zip_out_bytes(s, s->window + s->block_start, (size_t)raw_len);
        if (s->oom) return;
        s->block_start = s->strstart;
        s->token_count = 0;
        return;
    }

    if (s->token_count == 0) {
        /* Empty block: 10-bit fixed EOB (bfinal, BTYPE=01, 7-bit 0). */
        zip_put_bits(s, (uint32_t)(bfinal ? 3u : 2u), 3);
        zip_put_bits(s, 0u, 7);
        if (s->oom) return;
        s->block_start = s->strstart;
        return;
    }

    uint32_t lit_freq[286] = {0};
    uint32_t dist_freq[30] = {0};
    uint32_t extra_bits = 0;
    for (int i = 0; i < s->token_count; i++) {
        zip_token t = s->tokens[i];
        if (t.dist == 0) {
            lit_freq[t.lit_len]++;
        } else {
            int l_idx = zip_len_code_idx((int)t.lit_len);
            int d_idx = zip_dist_code_idx((int)t.dist);
            lit_freq[257 + l_idx]++;
            dist_freq[d_idx]++;
            extra_bits += zip_len_extra[l_idx] + zip_dist_extra[d_idx];
        }
    }
    lit_freq[256] = 1;

    /* Fixed Huffman cost */
    uint32_t fixed_bits = 3u + extra_bits;
    for (int i = 0; i < 286; i++) {
        if (lit_freq[i]) fixed_bits += lit_freq[i] * zip_fixed_lit_len(i);
    }
    for (int i = 0; i < 30; i++) {
        if (dist_freq[i]) fixed_bits += dist_freq[i] * 5u;
    }

    /* Stored block cost */
    uint32_t align_bits = (uint32_t)((8 - ((s->bitcnt + 3) & 7)) & 7);
    uint32_t stored_bits = 3u + align_bits + 32u + (uint32_t)raw_len * 8u;

    /* Dynamic Huffman build & cost */
    uint8_t lit_lens[286], dist_lens[30], cl_lens[19];
    zip_build_tree_lengths(lit_freq, 286, 15, lit_lens, 0);
    zip_build_tree_lengths(dist_freq, 30, 15, dist_lens, 0);

    int hlit = 286;
    while (hlit > 257 && lit_lens[hlit - 1] == 0) hlit--;
    int hdist = 30;
    while (hdist > 1 && dist_lens[hdist - 1] == 0) hdist--;

    uint8_t combined[286 + 30];
    memcpy(combined, lit_lens, (size_t)hlit);
    memcpy(combined + hlit, dist_lens, (size_t)hdist);

    uint8_t rle_sym[316], rle_extra[316];
    uint32_t cl_freq[19];
    int rle_n = zip_rle_lengths(combined, hlit + hdist, rle_sym, rle_extra, cl_freq);
    zip_build_tree_lengths(cl_freq, 19, 7, cl_lens, 1);

    int hclen = 19;
    while (hclen > 4 && cl_lens[zip_cl_order[hclen - 1]] == 0) hclen--;

    uint32_t dyn_bits = 3u + 5u + 5u + 4u + (uint32_t)hclen * 3u + extra_bits;
    for (int i = 0; i < 19; i++) {
        if (cl_freq[i]) dyn_bits += cl_freq[i] * cl_lens[i];
    }
    dyn_bits += cl_freq[16] * 2u + cl_freq[17] * 3u + cl_freq[18] * 7u;
    for (int i = 0; i < hlit; i++) dyn_bits += lit_freq[i] * lit_lens[i];
    for (int i = 0; i < hdist; i++) dyn_bits += dist_freq[i] * dist_lens[i];

    if (stored_bits <= fixed_bits && stored_bits <= dyn_bits) {
        zip_put_bits(s, (uint32_t)(bfinal ? 1u : 0u), 3);
        zip_flush_bits(s);
        zip_out_u16_le(s, (uint16_t)raw_len);
        zip_out_u16_le(s, (uint16_t)(~(uint32_t)raw_len));
        zip_out_bytes(s, s->window + s->block_start, (size_t)raw_len);
    } else if (fixed_bits <= dyn_bits) {
        uint16_t f_lit_codes[286], f_dist_codes[30];
        uint8_t f_lit_lens[286], f_dist_lens[30];
        for (int i = 0; i < 286; i++) {
            f_lit_lens[i] = (uint8_t)zip_fixed_lit_len(i);
            f_lit_codes[i] = zip_fixed_lit_code(i);
        }
        for (int i = 0; i < 30; i++) {
            f_dist_lens[i] = 5u;
            f_dist_codes[i] = zip_fixed_dist_code(i);
        }
        zip_put_bits(s, (uint32_t)((bfinal ? 1u : 0u) | (1u << 1)), 3);
        zip_emit_tokens(s, f_lit_codes, f_lit_lens, f_dist_codes, f_dist_lens);
    } else {
        uint16_t cl_codes[19], lit_codes[286], dist_codes[30];
        zip_gen_codes(cl_lens, 19, cl_codes);
        zip_gen_codes(lit_lens, hlit, lit_codes);
        zip_gen_codes(dist_lens, hdist, dist_codes);

        zip_put_bits(s, (uint32_t)((bfinal ? 1u : 0u) | (2u << 1)), 3);
        zip_put_bits(s, (uint32_t)(hlit - 257), 5);
        zip_put_bits(s, (uint32_t)(hdist - 1), 5);
        zip_put_bits(s, (uint32_t)(hclen - 4), 4);
        for (int i = 0; i < hclen; i++) {
            zip_put_bits(s, (uint32_t)cl_lens[zip_cl_order[i]], 3);
        }
        for (int i = 0; i < rle_n; i++) {
            int sym = (int)rle_sym[i];
            zip_put_bits(s, cl_codes[sym], (int)cl_lens[sym]);
            if (sym == 16) zip_put_bits(s, rle_extra[i], 2);
            else if (sym == 17) zip_put_bits(s, rle_extra[i], 3);
            else if (sym == 18) zip_put_bits(s, rle_extra[i], 7);
        }
        zip_emit_tokens(s, lit_codes, lit_lens, dist_codes, dist_lens);
    }

    if (s->oom) return;
    s->block_start = s->strstart;
    s->token_count = 0;
}

static uint32_t zip_hash3(const uint8_t *p) {
    return (((uint32_t)p[0] << 10) ^ ((uint32_t)p[1] << 5) ^ (uint32_t)p[2]) & ZIP_DEF_HASH_MASK;
}

static int32_t zip_insert_hash(zip_deflater *s, int32_t pos) {
    uint32_t h = zip_hash3(s->window + pos);
    int32_t prev_head = s->head[h];
    s->prev[pos & ZIP_WIN_MASK] = prev_head;
    s->head[h] = pos;
    return prev_head;
}

/* zlib's configuration_table for the lazy levels (4-9): once the pending match is at least
 * `good` long, the next search walks a quarter of the hash chain; once it is at least `lazy`
 * long, there is no next search at all. Repetitive input (Minecraft's chunk data, which the
 * 1.7.10 server compresses at level 6 for every chunk packet) is mostly long matches, where
 * these two cut most of the work. Levels 1-3 do not search lazily, so theirs are unused. */
static const int32_t zip_good_tbl[10] = {0, 0, 0, 0, 4, 8, 8, 8, 32, 32};
static const int32_t zip_lazy_tbl[10] = {0, 0, 0, 0, 4, 16, 16, 32, 128, 258};

static int32_t zip_longest_match(zip_deflater *s, int32_t cur, int32_t *best_dist) {
    static const int32_t chain_tbl[10] = {0, 4, 8, 16, 16, 32, 128, 256, 1024, 4096};
    static const int32_t nice_tbl[10]  = {0, 8, 16, 32, 32, 64, 128, 128, 258, 258};

    int32_t max_len = s->lookahead;
    if (max_len > ZIP_DEF_MAX_MATCH) max_len = ZIP_DEF_MAX_MATCH;
    int32_t min_match = (s->strategy == ZIP_STRAT_FILTERED) ? 4 : 3;
    if (max_len < min_match) return 0;

    int32_t limit = (s->strstart > 32767) ? (s->strstart - 32767) : 0;
    int32_t chain = chain_tbl[s->level];
    if (s->prev_avail && s->prev_len >= zip_good_tbl[s->level]) chain >>= 2;
    int32_t nice = nice_tbl[s->level];
    if (nice > max_len) nice = max_len;

    const uint8_t *scan = s->window + s->strstart;
    int32_t best_len = min_match - 1;

    while (cur >= limit && cur < s->strstart && chain-- > 0) {
        const uint8_t *match = s->window + cur;
        if (match[best_len] == scan[best_len] && match[0] == scan[0] && match[1] == scan[1]) {
            int32_t len = 2;
            while (len < max_len && match[len] == scan[len]) len++;
            if (len > best_len) {
                best_len = len;
                *best_dist = s->strstart - cur;
                if (best_len >= nice) break;
            }
        }
        int32_t next = s->prev[cur & ZIP_WIN_MASK];
        if (next >= cur) break;
        cur = next;
    }

    return (best_len >= min_match) ? best_len : 0;
}

static void zip_slide_window(zip_deflater *s) {
    int32_t rem = s->strstart + s->lookahead - (int32_t)ZIP_WIN_SIZE;
    memmove(s->window, s->window + ZIP_WIN_SIZE, (size_t)rem);
    s->strstart -= (int32_t)ZIP_WIN_SIZE;
    s->block_start -= (int32_t)ZIP_WIN_SIZE;
    for (int i = 0; i < ZIP_DEF_HASH_SIZE; i++) {
        s->head[i] = (s->head[i] >= (int32_t)ZIP_WIN_SIZE) ? (s->head[i] - (int32_t)ZIP_WIN_SIZE) : -1;
    }
    for (int i = 0; i < (int)ZIP_WIN_SIZE; i++) {
        s->prev[i] = (s->prev[i] >= (int32_t)ZIP_WIN_SIZE) ? (s->prev[i] - (int32_t)ZIP_WIN_SIZE) : -1;
    }
}

static void zip_resolve_prev(zip_deflater *s) {
    if (!s->prev_avail) return;
    s->tokens[s->token_count++] = (zip_token){(uint16_t)s->prev_dist, (uint16_t)s->prev_len};
    int32_t skip = s->prev_len - 1;
    for (int32_t i = 1; i < skip; i++) {
        if (s->lookahead - i >= 3) zip_insert_hash(s, s->strstart + i);
    }
    s->strstart += skip;
    s->lookahead -= skip;
    s->prev_avail = 0;
    s->prev_len = 2;
}

static void zip_maybe_flush_block(zip_deflater *s) {
    if (s->oom) return;
    if (s->level == 0) {
        if (s->strstart - s->block_start >= (int32_t)ZIP_WIN_SIZE) {
            zip_flush_block(s, 0);
        }
    } else if (s->token_count >= ZIP_DEF_MAX_TOKENS - 1 ||
               s->strstart - s->block_start >= (int32_t)ZIP_WIN_SIZE ||
               s->strstart >= ZIP_DEF_WIN_CAP - ZIP_DEF_MAX_LOOK) {
        zip_resolve_prev(s);
        zip_flush_block(s, 0);
    }
    if (s->oom) return;
    if (s->strstart >= ZIP_DEF_WIN_CAP - ZIP_DEF_MAX_LOOK) {
        zip_slide_window(s);
    }
}

/* Compress available lookahead. If final_drain is 0, keep up to ZIP_DEF_MAX_LOOK
 * bytes for full match lookahead; if final_drain is 1, drain lookahead to 0. */
static void zip_compress_step(zip_deflater *s, int final_drain) {
    if (s->oom) return;
    if (s->level == 0) {
        while (s->lookahead > 0) {
            int32_t chunk = (int32_t)ZIP_WIN_SIZE - (s->strstart - s->block_start);
            if (chunk > s->lookahead) chunk = s->lookahead;
            s->strstart += chunk;
            s->lookahead -= chunk;
            zip_maybe_flush_block(s);
            if (s->oom) return;
        }
        return;
    }

    int32_t min_look = final_drain
        ? ((s->strategy == ZIP_STRAT_HUFFMAN) ? 1 : 3)
        : ZIP_DEF_MAX_LOOK;
    int use_lazy = (s->level >= 4 && s->strategy != ZIP_STRAT_HUFFMAN);

    while (s->lookahead >= min_look) {
        zip_maybe_flush_block(s);
        if (s->oom || s->lookahead < min_look) break;

        if (s->strategy == ZIP_STRAT_HUFFMAN || s->lookahead < 3) {
            s->tokens[s->token_count++] = (zip_token){0u, (uint16_t)s->window[s->strstart]};
            if (s->strategy != ZIP_STRAT_HUFFMAN && s->lookahead >= 3) {
                zip_insert_hash(s, s->strstart);
            }
            s->strstart++;
            s->lookahead--;
            continue;
        }

        int32_t hash_head = zip_insert_hash(s, s->strstart);
        int32_t cur_dist = 0;
        /* A pending match already as long as zlib's max_lazy is taken without looking further. */
        int skip_search = use_lazy && s->prev_avail && s->prev_len >= zip_lazy_tbl[s->level];
        int32_t cur_len = (hash_head >= 0 && !skip_search) ? zip_longest_match(s, hash_head, &cur_dist) : 0;

        if (use_lazy) {
            if (s->prev_avail) {
                if (cur_len > s->prev_len) {
                    s->tokens[s->token_count++] = (zip_token){0u, (uint16_t)s->window[s->strstart - 1]};
                    s->prev_len = cur_len;
                    s->prev_dist = cur_dist;
                    s->strstart++;
                    s->lookahead--;
                } else {
                    zip_resolve_prev(s);
                }
            } else if (cur_len >= 3) {
                s->prev_avail = 1;
                s->prev_len = cur_len;
                s->prev_dist = cur_dist;
                s->strstart++;
                s->lookahead--;
            } else {
                s->tokens[s->token_count++] = (zip_token){0u, (uint16_t)s->window[s->strstart]};
                s->strstart++;
                s->lookahead--;
            }
        } else {
            if (cur_len >= 3) {
                s->tokens[s->token_count++] = (zip_token){(uint16_t)cur_dist, (uint16_t)cur_len};
                for (int32_t i = 1; i < cur_len; i++) {
                    if (s->lookahead - i >= 3) zip_insert_hash(s, s->strstart + i);
                }
                s->strstart += cur_len;
                s->lookahead -= cur_len;
            } else {
                s->tokens[s->token_count++] = (zip_token){0u, (uint16_t)s->window[s->strstart]};
                s->strstart++;
                s->lookahead--;
            }
        }
    }

    if (final_drain && !s->oom) {
        zip_resolve_prev(s);
        while (s->lookahead > 0 && !s->oom) {
            zip_maybe_flush_block(s);
            if (s->oom) return;
            s->tokens[s->token_count++] = (zip_token){0u, (uint16_t)s->window[s->strstart++]};
            s->lookahead--;
        }
    }
}

static int32_t zip_drain_staging(zip_deflater *s, uint8_t *out, int32_t out_len) {
    size_t avail = s->out_tail - s->out_head;
    if (avail == 0 || out_len <= 0) return 0;
    size_t n = avail < (size_t)out_len ? avail : (size_t)out_len;
    memcpy(out, s->out_buf + s->out_head, n);
    s->out_head += n;
    if (s->out_head == s->out_tail) {
        s->out_head = 0;
        s->out_tail = 0;
    }
    return (int32_t)n;
}

int64_t jabs_zip_deflate_init(int32_t level, int32_t strategy, int32_t nowrap) {
    if (level != -1 && (level < 0 || level > 9)) {
        jabs_zip_illegal_argument("invalid compression level");
        return 0;
    }
    if (strategy < 0 || strategy > 2) {
        jabs_zip_illegal_argument("invalid compression strategy");
        return 0;
    }
    zip_deflater *s = (zip_deflater *)calloc(1, sizeof(zip_deflater));
    if (!s) {
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return 0;
    }
    s->level = (level == -1) ? 6 : level;
    s->strategy = strategy;
    s->nowrap = nowrap ? 1 : 0;
    jabs_zip_deflate_reset((int64_t)(intptr_t)s);
    return (int64_t)(intptr_t)s;
}

void jabs_zip_deflate_reset(int64_t stream) {
    zip_deflater *s = (zip_deflater *)(intptr_t)stream;
    if (!s) return;
    s->header_written = 0;
    s->has_dict = 0;
    s->finished = 0;
    s->oom = 0;
    s->last_flush = ZIP_DEF_NO_FLUSH;
    s->adler = 1u;
    s->bitbuf = 0;
    s->bitcnt = 0;
    s->out_head = 0;
    s->out_tail = 0;
    s->strstart = 0;
    s->block_start = 0;
    s->lookahead = 0;
    s->prev_avail = 0;
    s->prev_len = 2;
    s->prev_dist = 0;
    s->token_count = 0;
    memset(s->head, 0xff, sizeof(s->head));
    memset(s->prev, 0xff, sizeof(s->prev));
}

void jabs_zip_deflate_end(int64_t stream) {
    zip_deflater *s = (zip_deflater *)(intptr_t)stream;
    if (!s) return;
    free(s->out_buf);
    free(s);
}

int32_t jabs_zip_deflate_adler(int64_t stream) {
    zip_deflater *s = (zip_deflater *)(intptr_t)stream;
    return s ? (int32_t)s->adler : 0;
}

void jabs_zip_deflate_set_dict(int64_t stream, const uint8_t *dict, int32_t len) {
    zip_deflater *s = (zip_deflater *)(intptr_t)stream;
    if (!s) return;
    if (len < 0 || (dict == NULL && len != 0)) {
        jabs_zip_illegal_argument("invalid dictionary");
        return;
    }
    if (s->header_written || s->finished || s->strstart > 0 || s->lookahead > 0) {
        jabs_zip_illegal_argument("dictionary not allowed after deflate");
        return;
    }
    if (!s->nowrap) {
        s->adler = jabs_adler32(s->adler, dict, (size_t)len);
        s->has_dict = 1;
    }
    size_t copy_len = (size_t)len;
    const uint8_t *src = dict;
    if (copy_len > ZIP_WIN_SIZE) {
        src += copy_len - ZIP_WIN_SIZE;
        copy_len = ZIP_WIN_SIZE;
    }
    if (copy_len > 0) {
        memcpy(s->window, src, copy_len);
        if (s->level > 0 && s->strategy != ZIP_STRAT_HUFFMAN && copy_len >= 3u) {
            for (size_t i = 0; i <= copy_len - 3u; i++) {
                zip_insert_hash(s, (int32_t)i);
            }
        }
        s->strstart = (int32_t)copy_len;
        s->block_start = (int32_t)copy_len;
    }
}

int64_t jabs_zip_deflate(int64_t stream, const uint8_t *in, int32_t in_len, uint8_t *out, int32_t out_len, int32_t flush, int32_t params) {
    zip_deflater *s = (zip_deflater *)(intptr_t)stream;
    if (!s) return 0;
    if (s->oom) {
        if (!jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return 0;
    }
    if (in_len < 0) in_len = 0;
    if (out_len < 0) out_len = 0;

    int params_active = (params & 1) != 0;
    if (params_active) {
        int32_t new_strat = (int32_t)(((uint32_t)params >> 1) & 3u);
        int32_t new_level = (params < 0)
            ? (int32_t)(((uint32_t)params >> 3) | 0xe0000000u)
            : (int32_t)((uint32_t)params >> 3);
        if (new_level != -1 && (new_level < 0 || new_level > 9)) {
            jabs_zip_illegal_argument("invalid compression level");
            return 0;
        }
        if (new_strat < 0 || new_strat > 2) {
            jabs_zip_illegal_argument("invalid compression strategy");
            return 0;
        }
        if (new_level == -1) new_level = 6;
        if (new_level != s->level || new_strat != s->strategy) {
            if (!s->finished) {
                /* Flush buffered data under the old level/strategy before switching. */
                zip_compress_step(s, 1);
                if (s->token_count > 0 || s->strstart > s->block_start) {
                    zip_flush_block(s, 0);
                }
                if (s->oom) return 0;
            }
            s->level = new_level;
            s->strategy = new_strat;
            s->last_flush = ZIP_DEF_NO_FLUSH;
        }
    }

    int32_t out_pos = zip_drain_staging(s, out, out_len);
    if (s->out_tail > s->out_head) {
        int params_pending = params_active ? 1 : 0;
        return zip_pack_result(0, out_pos, 0, params_pending);
    }

    if (s->finished) {
        return zip_pack_result(0, out_pos, 1, 0);
    }

    int32_t in_pos = 0;

    /* Feed input into window in chunks while staging buffer is empty or output has room. */
    while (in_pos < in_len && (s->out_tail == s->out_head || out_pos < out_len)) {
        if (s->strstart >= ZIP_DEF_WIN_CAP - ZIP_DEF_MAX_LOOK) {
            zip_maybe_flush_block(s);
            if (s->oom) return 0;
        }
        int32_t space = ZIP_DEF_WIN_CAP - (s->strstart + s->lookahead);
        if (space <= 0) {
            zip_compress_step(s, 0);
            if (s->oom) return 0;
            continue;
        }
        int32_t take = in_len - in_pos;
        if (take > space) take = space;
        memcpy(s->window + s->strstart + s->lookahead, in + in_pos, (size_t)take);
        zip_write_zlib_header(s);
        if (s->oom) return 0;
        if (!s->nowrap) {
            s->adler = jabs_adler32(s->adler, in + in_pos, (size_t)take);
        }
        s->lookahead += take;
        in_pos += take;
        s->last_flush = ZIP_DEF_NO_FLUSH;
        zip_compress_step(s, 0);
        if (s->oom) return 0;
        if (out_pos < out_len && s->out_tail > s->out_head) {
            out_pos += zip_drain_staging(s, out + out_pos, out_len - out_pos);
        }
        if (s->out_tail > s->out_head && out_pos >= out_len) break;
    }

    if (in_pos == in_len) {
        if (flush == ZIP_DEF_SYNC_FLUSH || flush == ZIP_DEF_FULL_FLUSH) {
            if (in_pos > 0 || s->last_flush < flush || !s->header_written) {
                zip_compress_step(s, 1);
                if (s->token_count > 0 || s->strstart > s->block_start || !s->header_written) {
                    zip_flush_block(s, 0);
                }
                /* Emit empty stored block aligned to byte boundary for Z_SYNC_FLUSH. */
                zip_put_bits(s, 0u, 3);
                zip_flush_bits(s);
                zip_out_u16_le(s, 0x0000u);
                zip_out_u16_le(s, 0xffffu);
                if (s->oom) return 0;
                if (flush == ZIP_DEF_FULL_FLUSH) {
                    memset(s->head, 0xff, sizeof(s->head));
                    memset(s->prev, 0xff, sizeof(s->prev));
                    s->strstart = 0;
                    s->block_start = 0;
                    s->lookahead = 0;
                }
                s->last_flush = flush;
            }
        } else if (flush == ZIP_DEF_FINISH) {
            zip_compress_step(s, 1);
            zip_flush_block(s, 1);
            zip_flush_bits(s);
            if (!s->nowrap) {
                zip_out_u32_be(s, s->adler);
            }
            if (s->oom) return 0;
            s->finished = 1;
            s->last_flush = flush;
        }
    }

    if (out_pos < out_len && s->out_tail > s->out_head) {
        out_pos += zip_drain_staging(s, out + out_pos, out_len - out_pos);
    }

    int is_finished = s->finished && (s->out_head == s->out_tail);
    int params_pending = (params_active && s->out_tail > s->out_head) ? 1 : 0;
    return zip_pack_result(in_pos, out_pos, is_finished, params_pending);
}
