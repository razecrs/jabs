// Objects of one class: constructors, fields at every width, a self-referencing
// linked list, virtual/private/final calls, null handling and NullPointerException.
public class Node {
    int value;
    Node next;
    long weight;
    byte tag;
    boolean marked;
    double scale = 0.25;

    Node() {}

    Node(int value, Node next) {
        this.value = value;
        this.next = next;
        this.weight = value * 3L;
        this.tag = (byte) value;
        this.marked = (value & 1) == 1;
    }

    int sum() {
        int total = 0;
        for (Node n = this; n != null; n = n.next) total += n.value;
        return total;
    }

    int length() { return next == null ? 1 : 1 + next.length(); }
    private int twice() { return value * 2; }
    final long heavier(long extra) { return weight + extra; }

    static Node chain(int count) {
        Node head = null;
        for (int i = 1; i <= count; i++) head = new Node(i, head);
        return head;
    }

    static int build(int count) { return chain(count).sum(); }
    static int lengthOf(int count) { return chain(count).length(); }
    static long totalWeight(int count) {
        long w = 0;
        for (Node n = chain(count); n != null; n = n.next) w = n.heavier(w);
        return w;
    }
    static int tagOf(int v) { return new Node(v, null).tag; }
    static int marked(int v) { return new Node(v, null).marked ? 1 : 0; }
    static int privateCall(int v) { return new Node(v, null).twice(); }
    static int defaults() {
        Node n = new Node();
        return n.value + (n.next == null ? 100 : 0) + (int) n.weight + n.tag + (n.marked ? 1 : 0) + (int) (n.scale * 4);
    }
    static int sameObject(int v) {
        Node a = new Node(v, null);
        Node b = a;
        Node c = new Node(v, null);
        return (a == b ? 1 : 0) + (a != c ? 10 : 0);
    }
    static int nullField() { Node n = chain(0); return n.value; }
    static int nullCall(int c) { Node n = c > 0 ? new Node(c, null) : null; return n.length(); }
    static int relink(int count) {
        Node head = chain(count);
        head.next.next = null;
        head.value = -head.value;
        return head.length() * 1000 + head.sum();
    }
}
