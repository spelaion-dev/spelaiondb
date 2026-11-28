-- The read-only tests' user, in FREEPDB1: `spelaion_ro`, password `spelaion` (ADR-080). A local
-- user that the database itself keeps read-only (`ALTER USER … READ ONLY`, 23ai and later): its
-- sessions are refused every write with ORA-28194, whatever function, view or job the write
-- hides in. It may read the writable user's `spelaion_ro_reads`; tests grant it more on their
-- own objects. Run by the image on every start, after 01-spelaion.sql, so it must work when the
-- user and the table exist, and it puts READ ONLY back if a test or a person took it away.
alter session set container = FREEPDB1;
declare
  existing number;
begin
  select count(*) into existing from dba_users where username = 'SPELAION_RO';
  if existing = 0 then
    execute immediate 'create user spelaion_ro identified by spelaion';
    execute immediate 'grant create session to spelaion_ro';
  end if;
  execute immediate 'alter user spelaion_ro read only';

  select count(*) into existing from dba_tables
    where owner = 'SPELAION' and table_name = 'SPELAION_RO_READS';
  if existing = 0 then
    execute immediate 'create table spelaion.spelaion_ro_reads (id number(9) primary key, note varchar2(40))';
    execute immediate q'[insert into spelaion.spelaion_ro_reads values (1, 'read')]';
    commit;
  end if;
  execute immediate 'grant select on spelaion.spelaion_ro_reads to spelaion_ro';
end;
/
