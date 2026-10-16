-- Schema of the order domain. The limits below mirror src/domain (and the limits in src/app): the database refuses what the
-- domain would refuse, so a bypass of the application layer cannot store invalid data.

CREATE TABLE customers (
    id              uuid         PRIMARY KEY DEFAULT gen_random_uuid(),
    name            varchar(200) NOT NULL,
    tax_id          varchar(20)  NOT NULL,
    passport_number varchar(9),
    email           varchar(320) NOT NULL,
    version         bigint       NOT NULL DEFAULT 1,
    created_at      timestamptz  NOT NULL,
    updated_at      timestamptz  NOT NULL,
    created_by      varchar(200) NOT NULL,
    updated_by      varchar(200) NOT NULL,
    deleted_at      timestamptz,
    deleted_by      varchar(200),
    CONSTRAINT customers_name_not_blank CHECK (btrim(name) <> ''),
    CONSTRAINT customers_soft_delete_audit CHECK ((deleted_at IS NULL) = (deleted_by IS NULL))
);

-- Uniqueness only among active rows: a soft-deleted customer releases its tax id and passport.
CREATE UNIQUE INDEX customers_tax_id_active_uq ON customers (tax_id) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX customers_passport_active_uq ON customers (passport_number)
    WHERE deleted_at IS NULL AND passport_number IS NOT NULL;

CREATE TABLE orders (
    id          uuid         PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id uuid         NOT NULL REFERENCES customers (id),
    status      varchar(20)  NOT NULL,
    version     bigint       NOT NULL DEFAULT 1,
    created_at  timestamptz  NOT NULL,
    updated_at  timestamptz  NOT NULL,
    created_by  varchar(200) NOT NULL,
    updated_by  varchar(200) NOT NULL,
    deleted_at  timestamptz,
    deleted_by  varchar(200),
    CONSTRAINT orders_status_valid CHECK (status IN ('OPEN', 'CONFIRMED', 'CANCELED')),
    CONSTRAINT orders_soft_delete_audit CHECK ((deleted_at IS NULL) = (deleted_by IS NULL))
);

CREATE INDEX orders_customer_active_idx ON orders (customer_id) WHERE deleted_at IS NULL;
CREATE INDEX orders_created_at_idx ON orders (created_at DESC, id);

CREATE TABLE order_items (
    id          uuid          PRIMARY KEY DEFAULT gen_random_uuid(),
    seq         bigint        GENERATED ALWAYS AS IDENTITY,
    order_id    uuid          NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    description varchar(255)  NOT NULL,
    unit_price  numeric(10,2) NOT NULL,
    quantity    integer       NOT NULL,
    CONSTRAINT order_items_description_not_blank CHECK (btrim(description) <> ''),
    CONSTRAINT order_items_unit_price_positive CHECK (unit_price > 0),
    CONSTRAINT order_items_quantity_range CHECK (quantity BETWEEN 1 AND 10000)
);

CREATE INDEX order_items_order_idx ON order_items (order_id, seq);
