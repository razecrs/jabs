import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;

// Byte-array view VarHandles (MethodHandles.byteArrayViewVarHandle): a byte[] read and written
// as shorts, chars, ints, longs, floats and doubles at any byte offset, in both byte orders,
// with the JVM's checks (ClassCastException, NullPointerException, ArrayIndexOutOfBounds).
// The JDK's DataInputStream/DataOutputStream reach the same handles through
// jdk.internal.util.ByteArray. If these differ from the JVM, look at jabs_vh_address
// (JABS_VH_BYTE_VIEW) and jabs_var_handle_byte_view in src/cgen.rs, byte_view_access in
// src/cgen.rs, and jabs_vh_view_load/jabs_vh_view_store in runtime/jabs_rt.h.
public class Views {
    static final VarHandle SHORT_BE = MethodHandles.byteArrayViewVarHandle(short[].class, ByteOrder.BIG_ENDIAN);
    static final VarHandle SHORT_LE = MethodHandles.byteArrayViewVarHandle(short[].class, ByteOrder.LITTLE_ENDIAN);
    static final VarHandle CHAR_BE = MethodHandles.byteArrayViewVarHandle(char[].class, ByteOrder.BIG_ENDIAN);
    static final VarHandle CHAR_LE = MethodHandles.byteArrayViewVarHandle(char[].class, ByteOrder.LITTLE_ENDIAN);
    static final VarHandle INT_BE = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
    static final VarHandle INT_LE = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
    static final VarHandle LONG_BE = MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.BIG_ENDIAN);
    static final VarHandle LONG_LE = MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN);
    static final VarHandle FLOAT_BE = MethodHandles.byteArrayViewVarHandle(float[].class, ByteOrder.BIG_ENDIAN);
    static final VarHandle FLOAT_LE = MethodHandles.byteArrayViewVarHandle(float[].class, ByteOrder.LITTLE_ENDIAN);
    static final VarHandle DOUBLE_BE = MethodHandles.byteArrayViewVarHandle(double[].class, ByteOrder.BIG_ENDIAN);
    static final VarHandle DOUBLE_LE = MethodHandles.byteArrayViewVarHandle(double[].class, ByteOrder.LITTLE_ENDIAN);

    /** Ten distinct bytes with high bits set, so sign extension and byte order both show. */
    static byte[] pattern() {
        byte[] b = new byte[10];
        for (int i = 0; i < b.length; i++) b[i] = (byte) (0x81 + i * 37);
        return b;
    }

    /** Every byte of an array folded into one long, position-sensitive. */
    static long fold(byte[] b) {
        long h = b.length;
        for (byte x : b) h = h * 1_000_003L + (x & 0xff);
        return h;
    }

    // Reads at a byte offset (unaligned ones included; the last legal offset is length - size).
    static int viewShortBigEndianGet(int offset) { return (short) SHORT_BE.get(pattern(), offset); }
    static int viewShortLittleEndianGet(int offset) { return (short) SHORT_LE.get(pattern(), offset); }
    static int viewCharBigEndianGet(int offset) { return (char) CHAR_BE.get(pattern(), offset); }
    static int viewCharLittleEndianGet(int offset) { return (char) CHAR_LE.get(pattern(), offset); }
    static int viewIntBigEndianGet(int offset) { return (int) INT_BE.get(pattern(), offset); }
    static int viewIntLittleEndianGet(int offset) { return (int) INT_LE.get(pattern(), offset); }
    static long viewLongBigEndianGet(int offset) { return (long) LONG_BE.get(pattern(), offset); }
    static long viewLongLittleEndianGet(int offset) { return (long) LONG_LE.get(pattern(), offset); }
    /** Raw bits, so a NaN payload read from the bytes is compared exactly. */
    static int viewFloatBigEndianGet(int offset) { return Float.floatToRawIntBits((float) FLOAT_BE.get(pattern(), offset)); }
    static int viewFloatLittleEndianGet(int offset) { return Float.floatToRawIntBits((float) FLOAT_LE.get(pattern(), offset)); }
    static long viewDoubleBigEndianGet(int offset) { return Double.doubleToRawLongBits((double) DOUBLE_BE.get(pattern(), offset)); }
    static long viewDoubleLittleEndianGet(int offset) { return Double.doubleToRawLongBits((double) DOUBLE_LE.get(pattern(), offset)); }

    // Writes at a byte offset into a copy of the pattern; the result shows every byte afterwards.
    static long viewShortBigEndianSet(int offset) { byte[] b = pattern(); SHORT_BE.set(b, offset, (short) 0x8a5b); return fold(b); }
    static long viewShortLittleEndianSet(int offset) { byte[] b = pattern(); SHORT_LE.set(b, offset, (short) 0x8a5b); return fold(b); }
    static long viewCharBigEndianSet(int offset) { byte[] b = pattern(); CHAR_BE.set(b, offset, (char) 0xfe01); return fold(b); }
    static long viewCharLittleEndianSet(int offset) { byte[] b = pattern(); CHAR_LE.set(b, offset, (char) 0xfe01); return fold(b); }
    static long viewIntBigEndianSet(int offset) { byte[] b = pattern(); INT_BE.set(b, offset, 0xdeadbeef); return fold(b); }
    static long viewIntLittleEndianSet(int offset) { byte[] b = pattern(); INT_LE.set(b, offset, 0xdeadbeef); return fold(b); }
    static long viewLongBigEndianSet(int offset) { byte[] b = pattern(); LONG_BE.set(b, offset, 0x0123456789abcdefL); return fold(b); }
    static long viewLongLittleEndianSet(int offset) { byte[] b = pattern(); LONG_LE.set(b, offset, 0x0123456789abcdefL); return fold(b); }
    /** A signalling NaN with a payload: the view stores the raw bits, unlike floatToIntBits. */
    static long viewFloatBigEndianSet(int offset) { byte[] b = pattern(); FLOAT_BE.set(b, offset, Float.intBitsToFloat(0x7f812345)); return fold(b); }
    static long viewFloatLittleEndianSet(int offset) { byte[] b = pattern(); FLOAT_LE.set(b, offset, -1.5f); return fold(b); }
    static long viewDoubleBigEndianSet(int offset) { byte[] b = pattern(); DOUBLE_BE.set(b, offset, Math.PI); return fold(b); }
    static long viewDoubleLittleEndianSet(int offset) { byte[] b = pattern(); DOUBLE_LE.set(b, offset, Double.longBitsToDouble(0xfff0000000000123L)); return fold(b); }

    // Misuse, each failing the way the JVM fails.
    /** Null array: NullPointerException (after the byte[] cast, which null passes). */
    static int viewGetOnNullArray() { byte[] b = null; return (int) INT_BE.get(b, 0); }
    /** Not a byte[]: ClassCastException, even at an index that would be out of bounds. */
    static int viewGetOnIntArray(int offset) { Object o = new int[4]; return (int) INT_BE.get(o, offset); }
    /** Shorter than one element: every offset is out of bounds. */
    static int viewIntGetOnTwoBytes(int offset) { return (int) INT_LE.get(new byte[2], offset); }
    static long viewLongSetOnEmpty(int offset) { byte[] b = new byte[0]; LONG_BE.set(b, offset, 1L); return fold(b); }

    // DataOutputStream/DataInputStream, which go through jdk.internal.util.ByteArray's views.
    /** Every number DataOutputStream writes, read back by DataInputStream; also folds the bytes. */
    static long dataStreamRoundTrip(long seed) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeShort((int) seed);
        out.writeChar((int) (seed >>> 8));
        out.writeInt((int) (seed * 31));
        out.writeLong(seed * 0x9E3779B97F4A7C15L);
        out.writeFloat(Float.intBitsToFloat((int) seed));
        out.writeDouble(Double.longBitsToDouble(seed ^ 0x5555555555555555L));
        out.writeUTF("view " + seed);
        out.flush();
        byte[] written = bytes.toByteArray();
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(written));
        long h = fold(written);
        h = h * 31 + in.readShort();
        h = h * 31 + in.readChar();
        h = h * 31 + in.readInt();
        h = h * 31 + in.readLong();
        h = h * 31 + Float.floatToRawIntBits(in.readFloat());
        h = h * 31 + Double.doubleToRawLongBits(in.readDouble());
        h = h * 31 + in.readUTF().hashCode();
        return h;
    }

    /** readInt/readLong/readUnsignedShort over the pattern after skipping `skip` bytes; a short
     * tail ends in EOFException, as on the JVM. */
    static long dataStreamReadsPattern(int skip) throws IOException {
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(pattern()));
        in.skipBytes(skip);
        long h = in.readUnsignedShort();
        h = h * 31 + in.readInt();
        h = h * 31 + in.readShort();
        return h * 31 + (skip == 0 ? in.readUnsignedShort() : in.readLong());
    }
}
